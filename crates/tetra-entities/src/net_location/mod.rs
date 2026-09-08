//! TETRA Location Information Protocol (LIP) decoding helpers.
//!
//! FlowStation receives LIP as SDS Type-4 user data with protocol identifier 10 (0x0A).
//! This module decodes the location-bearing LIP PDUs into neutral position reports so
//! consumers such as the dashboard map and GeoAlarm do not need to parse SDS text.

use tetra_saps::control::enums::sds_user_data::SdsUserData;

const LIP_PROTOCOL_ID: u8 = 0x0A;

#[derive(Debug, Clone, PartialEq)]
pub struct TetraLipPosition {
    pub lat: f64,
    pub lon: f64,
    pub speed_kmh: Option<f32>,
    pub short_report: bool,
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_len: usize,
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8], bit_len: usize) -> Self {
        Self {
            bytes,
            bit_len: bit_len.min(bytes.len().saturating_mul(8)),
            pos: 0,
        }
    }

    fn read(&mut self, bits: usize) -> Option<u32> {
        if bits > 32 || self.pos.checked_add(bits)? > self.bit_len {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = self.bytes[self.pos / 8];
            let shift = 7 - (self.pos % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.pos += 1;
        }
        Some(value)
    }

    fn skip(&mut self, bits: usize) -> bool {
        match self.pos.checked_add(bits) {
            Some(next) if next <= self.bit_len => {
                self.pos = next;
                true
            }
            _ => false,
        }
    }
}

fn signed_field(raw: u32, bits: u32) -> i64 {
    let sign = 1u32 << (bits - 1);
    if raw & sign == 0 {
        i64::from(raw)
    } else {
        i64::from(raw) - (1i64 << bits)
    }
}

fn decode_lat(raw: u32) -> f64 {
    signed_field(raw, 24) as f64 * 180.0 / ((1u64 << 24) as f64)
}

fn decode_lon(raw: u32) -> f64 {
    // LIP longitude is a signed 25-bit field using the same 2^24 angular scale.
    signed_field(raw, 25) as f64 * 180.0 / ((1u64 << 24) as f64)
}

fn valid_coord(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

fn horizontal_velocity(raw: u32) -> f32 {
    if raw < 29 {
        raw as f32
    } else {
        16.0 * 1.038_f32.powi(raw as i32 - 13)
    }
}

/// Decode a TETRA LIP position carried in an SDS user-data field.
///
/// Supported location-bearing PDUs:
/// - Short Location Report (PDU type 0)
/// - Long Location Report (long-PDU extension 3) for the standard location
///   shapes whose first fields are longitude + latitude.
///
/// Other LIP control/request PDUs intentionally return `None`.
pub fn decode_tetra_lip_position(data: &SdsUserData) -> Option<TetraLipPosition> {
    let SdsUserData::Type4(len_bits, bytes) = data else {
        return None;
    };
    if *len_bits < 10 || bytes.first().copied()? != LIP_PROTOCOL_ID {
        return None;
    }

    let payload_bits = usize::from(*len_bits).saturating_sub(8);
    let mut bits = BitReader::new(bytes.get(1..)?, payload_bits);
    let pdu_type = bits.read(2)?;

    match pdu_type {
        0 => {
            // Short Location Report:
            // time elapsed(2), longitude(25), latitude(24), position error(3),
            // horizontal velocity(7), direction of travel(4), additional-data type(1).
            bits.skip(2)?;
            let lon = decode_lon(bits.read(25)?);
            let lat = decode_lat(bits.read(24)?);
            bits.skip(3)?;
            let speed = horizontal_velocity(bits.read(7)?);
            bits.skip(4)?;
            bits.skip(1)?;

            valid_coord(lat, lon).then_some(TetraLipPosition {
                lat,
                lon,
                speed_kmh: Some(speed),
                short_report: true,
            })
        }
        1 => {
            let extension = bits.read(4)?;
            if extension != 3 {
                return None;
            }

            // Long Location Report. Time data is variable-length.
            match bits.read(2)? {
                0 => {}
                1 => {
                    bits.skip(2)?;
                }
                2 => {
                    bits.skip(22)?;
                }
                _ => return None,
            }

            // Location shape. Shape 0 carries no coordinates. Standard location-bearing
            // shapes start with longitude(25) + latitude(24); the remaining uncertainty,
            // altitude and velocity fields are irrelevant to the map position.
            let shape = bits.read(4)?;
            if !matches!(shape, 1 | 2 | 3 | 4 | 5 | 6 | 7 | 9 | 10) {
                return None;
            }
            let lon = decode_lon(bits.read(25)?);
            let lat = decode_lat(bits.read(24)?);

            valid_coord(lat, lon).then_some(TetraLipPosition {
                lat,
                lon,
                speed_kmh: None,
                short_report: false,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_bits(out: &mut Vec<u8>, value: u32, count: usize) {
        for bit in (0..count).rev() {
            out.push(((value >> bit) & 1) as u8);
        }
    }

    fn pack(bits: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, bit) in bits.iter().copied().enumerate() {
            if bit != 0 {
                bytes[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        bytes
    }

    fn angular_raw(value: f64, bits: u32) -> u32 {
        let scaled = (value * ((1u64 << 24) as f64) / 180.0).round() as i64;
        let modulus = 1i64 << bits;
        scaled.rem_euclid(modulus) as u32
    }

    #[test]
    fn decodes_short_location_report() {
        let mut payload = Vec::new();
        push_bits(&mut payload, 0, 2); // short PDU
        push_bits(&mut payload, 0, 2); // time elapsed
        push_bits(&mut payload, angular_raw(18.6466, 25), 25);
        push_bits(&mut payload, angular_raw(54.3520, 24), 24);
        push_bits(&mut payload, 0, 3); // position error
        push_bits(&mut payload, 12, 7); // speed
        push_bits(&mut payload, 0, 4); // direction
        push_bits(&mut payload, 0, 1); // additional-data type

        let mut bytes = vec![LIP_PROTOCOL_ID];
        bytes.extend(pack(&payload));
        let sds = SdsUserData::Type4((8 + payload.len()) as u16, bytes);
        let p = decode_tetra_lip_position(&sds).expect("position");

        assert!((p.lat - 54.3520).abs() < 0.0001);
        assert!((p.lon - 18.6466).abs() < 0.0001);
        assert_eq!(p.speed_kmh, Some(12.0));
        assert!(p.short_report);
    }

    #[test]
    fn decodes_southern_and_western_coordinates() {
        let mut payload = Vec::new();
        push_bits(&mut payload, 0, 2);
        push_bits(&mut payload, 0, 2);
        push_bits(&mut payload, angular_raw(-58.3816, 25), 25);
        push_bits(&mut payload, angular_raw(-34.6037, 24), 24);
        push_bits(&mut payload, 0, 3);
        push_bits(&mut payload, 0, 7);
        push_bits(&mut payload, 0, 4);
        push_bits(&mut payload, 0, 1);

        let mut bytes = vec![LIP_PROTOCOL_ID];
        bytes.extend(pack(&payload));
        let sds = SdsUserData::Type4((8 + payload.len()) as u16, bytes);
        let p = decode_tetra_lip_position(&sds).expect("position");

        assert!((p.lat + 34.6037).abs() < 0.0001);
        assert!((p.lon + 58.3816).abs() < 0.0001);
    }

    #[test]
    fn rejects_non_lip_sds() {
        let sds = SdsUserData::Type4(16, vec![0x82, 0x00]);
        assert!(decode_tetra_lip_position(&sds).is_none());
    }
}
