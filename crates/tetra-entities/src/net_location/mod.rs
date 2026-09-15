//! TETRA Location Information Protocol (LIP) decoding helpers.
//!
//! FlowStation receives LIP as SDS Type-4 user data with protocol identifier 10 (0x0A).
//! This module decodes location-bearing LIP PDUs into neutral reports so every source ISSI
//! can be discovered at runtime. Consumers such as the dashboard map and GeoAlarm therefore
//! do not need a per-radio configuration list or their own binary LIP parser.

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};
use tetra_saps::control::enums::sds_user_data::SdsUserData;

const LIP_PROTOCOL_ID: u8 = 0x0A;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TetraLipReportType {
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TetraLipLocationShape {
    Point,
    Circle,
    Ellipse,
    PointWithAltitude,
    CircleWithAltitude,
    EllipseWithAltitude,
    CircleWithAltitudeAndUncertainty,
    EllipseWithAltitudeAndUncertainty,
    Arc,
    PointWithPositionError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TetraLipAltitudeReference {
    Wgs84,
    UserDefined,
}

/// The absolute time-of-position carried by a long report. LIP deliberately omits month and
/// year, so the wire fields are retained instead of pretending this is a complete timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub struct TetraLipTimeOfPosition {
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Fully decoded location information used by telemetry, the map and external consumers.
/// Optional fields reflect the selected LIP location shape and velocity type.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct TetraLipPosition {
    pub lat: f64,
    pub lon: f64,
    pub report_type: TetraLipReportType,
    pub location_shape: TetraLipLocationShape,
    /// Raw ETSI time-elapsed code: 0 means under 5 s, 1 under 5 min, 2 under 30 min,
    /// and 3 unknown or not applicable.
    pub time_elapsed: Option<u8>,
    pub time_of_position: Option<TetraLipTimeOfPosition>,
    /// Raw short-report/point-with-error code. Values 6 and 7 have no finite upper bound.
    pub position_error_code: Option<u8>,
    pub position_error_m: Option<f32>,
    pub horizontal_uncertainty_m: Option<f32>,
    pub ellipse_semi_major_m: Option<f32>,
    pub ellipse_semi_minor_m: Option<f32>,
    pub ellipse_angle_deg: Option<f32>,
    pub arc_inner_radius_m: Option<f32>,
    pub arc_outer_radius_m: Option<f32>,
    pub arc_start_deg: Option<f32>,
    pub arc_stop_deg: Option<f32>,
    pub confidence_percent: Option<f32>,
    pub altitude_m: Option<i32>,
    pub altitude_reference: Option<TetraLipAltitudeReference>,
    pub altitude_accuracy_m: Option<f32>,
    pub speed_kmh: Option<f32>,
    pub speed_accuracy_kmh: Option<f32>,
    pub vertical_speed_kmh: Option<f32>,
    pub vertical_speed_accuracy_kmh: Option<f32>,
    pub heading_deg: Option<f32>,
    pub heading_accuracy_deg: Option<f32>,
    pub acknowledgement_requested: bool,
    pub reason_for_sending: Option<u8>,
    pub user_defined_data: Option<u8>,
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
    signed_field(raw, 25) as f64 * 180.0 / ((1u64 << 24) as f64)
}

fn valid_coord(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

/// ETSI TS 100 392-18-1 clause 6.3.17. Code 126 means more than the returned
/// threshold, while 127 explicitly means unknown and is represented as None.
fn horizontal_velocity(raw: u32) -> Option<f32> {
    match raw {
        0..=28 => Some(raw as f32),
        29..=126 => Some(16.0 * 1.038_f32.powi(raw as i32 - 13)),
        _ => None,
    }
}

fn accuracy_from_code(raw: u32) -> Option<f32> {
    match raw {
        0 => Some(1.5),
        1 => Some(3.0),
        2 => Some(6.0),
        3 => Some(12.0),
        4 => Some(24.0),
        5 => Some(48.0),
        6 => Some(96.0),
        _ => None,
    }
}

fn altitude_accuracy_from_code(raw: u32) -> Option<f32> {
    match raw {
        0 => Some(1.0),
        1 => Some(2.0),
        2 => Some(5.0),
        3 => Some(15.0),
        4 => Some(50.0),
        5 => Some(150.0),
        6 => Some(300.0),
        _ => None,
    }
}

fn position_error_m(raw: u32) -> Option<f32> {
    match raw {
        0 => Some(2.0),
        1 => Some(20.0),
        2 => Some(200.0),
        3 => Some(2_000.0),
        4 => Some(20_000.0),
        5 => Some(200_000.0),
        _ => None,
    }
}

fn horizontal_uncertainty_m(raw: u32) -> Option<f32> {
    // ETSI: 2 * (1 + 0.2)^(K + 5) - 4. Code 63 is best effort.
    (raw < 63).then(|| 2.0 * 1.2_f32.powi(raw as i32 + 5) - 4.0)
}

fn confidence_percent(raw: u32) -> Option<f32> {
    match raw {
        0 => Some(50.0),
        1 => Some(68.0),
        2 => Some(80.0),
        3 => Some(90.0),
        4 => Some(95.0),
        5 => Some(99.0),
        6 => Some(99.9),
        _ => None,
    }
}

fn angle_deg(raw: u32) -> f32 {
    raw as f32 * 360.0 / 256.0
}

fn decode_altitude(raw: u32) -> (TetraLipAltitudeReference, Option<i32>) {
    let reference = if raw & 0x800 == 0 {
        TetraLipAltitudeReference::Wgs84
    } else {
        TetraLipAltitudeReference::UserDefined
    };
    let value = raw & 0x7ff;
    let altitude = match value {
        0 => None,
        1..=1201 => Some(value as i32 - 201),
        1202..=1926 => Some(1_000 + (value as i32 - 1201) * 2),
        1927..=2047 => Some(2_450 + (value as i32 - 1926) * 75),
        _ => None,
    };
    (reference, altitude)
}

fn empty_position(
    lat: f64,
    lon: f64,
    report_type: TetraLipReportType,
    location_shape: TetraLipLocationShape,
) -> TetraLipPosition {
    TetraLipPosition {
        lat,
        lon,
        report_type,
        location_shape,
        time_elapsed: None,
        time_of_position: None,
        position_error_code: None,
        position_error_m: None,
        horizontal_uncertainty_m: None,
        ellipse_semi_major_m: None,
        ellipse_semi_minor_m: None,
        ellipse_angle_deg: None,
        arc_inner_radius_m: None,
        arc_outer_radius_m: None,
        arc_start_deg: None,
        arc_stop_deg: None,
        confidence_percent: None,
        altitude_m: None,
        altitude_reference: None,
        altitude_accuracy_m: None,
        speed_kmh: None,
        speed_accuracy_kmh: None,
        vertical_speed_kmh: None,
        vertical_speed_accuracy_kmh: None,
        heading_deg: None,
        heading_accuracy_deg: None,
        acknowledgement_requested: false,
        reason_for_sending: None,
        user_defined_data: None,
    }
}

fn read_coords(bits: &mut BitReader<'_>) -> Option<(f64, f64)> {
    let lon = decode_lon(bits.read(25)?);
    let lat = decode_lat(bits.read(24)?);
    valid_coord(lat, lon).then_some((lat, lon))
}

fn read_altitude(bits: &mut BitReader<'_>, position: &mut TetraLipPosition) -> Option<()> {
    let (reference, altitude) = decode_altitude(bits.read(12)?);
    position.altitude_reference = Some(reference);
    position.altitude_m = altitude;
    Some(())
}

fn read_ellipse(bits: &mut BitReader<'_>, position: &mut TetraLipPosition) -> Option<()> {
    position.ellipse_semi_major_m = horizontal_uncertainty_m(bits.read(6)?);
    position.ellipse_semi_minor_m = horizontal_uncertainty_m(bits.read(6)?);
    position.ellipse_angle_deg = Some(angle_deg(bits.read(8)?));
    position.confidence_percent = confidence_percent(bits.read(3)?);
    Some(())
}

fn read_velocity_data(bits: &mut BitReader<'_>, position: &mut TetraLipPosition) -> Option<()> {
    let velocity_type = bits.read(3)?;
    if velocity_type == 0 {
        return Some(());
    }

    position.speed_kmh = horizontal_velocity(bits.read(7)?);
    if matches!(velocity_type, 2 | 4 | 6 | 7) {
        position.speed_accuracy_kmh = accuracy_from_code(bits.read(3)?);
    }
    if matches!(velocity_type, 3 | 4 | 7) {
        let raw = bits.read(8)?;
        let magnitude = horizontal_velocity(raw & 0x7f);
        position.vertical_speed_kmh = magnitude.map(|speed| if raw & 0x80 == 0 { speed } else { -speed });
    }
    if matches!(velocity_type, 4 | 7) {
        position.vertical_speed_accuracy_kmh = accuracy_from_code(bits.read(3)?);
    }
    if matches!(velocity_type, 5 | 6 | 7) {
        position.heading_deg = Some(angle_deg(bits.read(8)?));
    }
    if matches!(velocity_type, 6 | 7) {
        position.heading_accuracy_deg = accuracy_from_code(bits.read(3)?);
    }
    Some(())
}

fn read_additional_data(bits: &mut BitReader<'_>, position: &mut TetraLipPosition) -> Option<()> {
    let user_defined = bits.read(1)? != 0;
    let value = bits.read(8)? as u8;
    if user_defined {
        position.user_defined_data = Some(value);
    } else {
        position.reason_for_sending = Some(value);
    }
    Some(())
}

/// Decode a TETRA LIP position carried in an SDS user-data field.
///
/// Supported location-bearing PDUs:
/// - Short Location Report (PDU type 0), including age, accuracy, speed, heading and reason;
/// - Long Location Report (extension 3), including every standardized location shape and
///   velocity representation needed by the operator map.
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
            let time_elapsed = bits.read(2)? as u8;
            let (lat, lon) = read_coords(&mut bits)?;
            let position_error_code = bits.read(3)? as u8;
            let speed_kmh = horizontal_velocity(bits.read(7)?);
            let heading_deg = Some(bits.read(4)? as f32 * 22.5);

            let mut position = empty_position(
                lat,
                lon,
                TetraLipReportType::Short,
                TetraLipLocationShape::PointWithPositionError,
            );
            position.time_elapsed = Some(time_elapsed);
            position.position_error_code = Some(position_error_code);
            position.position_error_m = position_error_m(u32::from(position_error_code));
            position.speed_kmh = speed_kmh;
            position.heading_deg = heading_deg;
            read_additional_data(&mut bits, &mut position)?;
            Some(position)
        }
        1 => {
            if bits.read(4)? != 3 {
                return None;
            }

            match bits.read(2)? {
                0 => {
                    let shape = bits.read(4)?;
                    decode_long_after_shape(&mut bits, shape, None, None)
                }
                1 => {
                    let time_elapsed = Some(bits.read(2)? as u8);
                    let shape = bits.read(4)?;
                    decode_long_after_shape(&mut bits, shape, time_elapsed, None)
                }
                2 => {
                    let day = bits.read(5)? as u8;
                    let hour = bits.read(5)? as u8;
                    let minute = bits.read(6)? as u8;
                    let second = bits.read(6)? as u8;
                    let valid = (1..=31).contains(&day) && hour <= 23 && minute <= 59 && second <= 59;
                    let time = valid.then_some(TetraLipTimeOfPosition {
                        day,
                        hour,
                        minute,
                        second,
                    });
                    let shape = bits.read(4)?;
                    decode_long_after_shape(&mut bits, shape, None, time)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn decode_long_after_shape(
    bits: &mut BitReader<'_>,
    shape: u32,
    time_elapsed: Option<u8>,
    time_of_position: Option<TetraLipTimeOfPosition>,
) -> Option<TetraLipPosition> {
    let location_shape = match shape {
        1 => TetraLipLocationShape::Point,
        2 => TetraLipLocationShape::Circle,
        3 => TetraLipLocationShape::Ellipse,
        4 => TetraLipLocationShape::PointWithAltitude,
        5 => TetraLipLocationShape::CircleWithAltitude,
        6 => TetraLipLocationShape::EllipseWithAltitude,
        7 => TetraLipLocationShape::CircleWithAltitudeAndUncertainty,
        8 => TetraLipLocationShape::EllipseWithAltitudeAndUncertainty,
        9 => TetraLipLocationShape::Arc,
        10 => TetraLipLocationShape::PointWithPositionError,
        _ => return None,
    };
    let (lat, lon) = read_coords(bits)?;
    let mut position = empty_position(lat, lon, TetraLipReportType::Long, location_shape);
    position.time_elapsed = time_elapsed;
    position.time_of_position = time_of_position;

    match shape {
        1 => {}
        2 => position.horizontal_uncertainty_m = horizontal_uncertainty_m(bits.read(6)?),
        3 => read_ellipse(bits, &mut position)?,
        4 => read_altitude(bits, &mut position)?,
        5 => {
            position.horizontal_uncertainty_m = horizontal_uncertainty_m(bits.read(6)?);
            read_altitude(bits, &mut position)?;
        }
        6 => {
            position.ellipse_semi_major_m = horizontal_uncertainty_m(bits.read(6)?);
            position.ellipse_semi_minor_m = horizontal_uncertainty_m(bits.read(6)?);
            position.ellipse_angle_deg = Some(angle_deg(bits.read(8)?));
            read_altitude(bits, &mut position)?;
            position.confidence_percent = confidence_percent(bits.read(3)?);
        }
        7 => {
            position.horizontal_uncertainty_m = horizontal_uncertainty_m(bits.read(6)?);
            read_altitude(bits, &mut position)?;
            position.altitude_accuracy_m = altitude_accuracy_from_code(bits.read(3)?);
        }
        8 => {
            position.ellipse_semi_major_m = horizontal_uncertainty_m(bits.read(6)?);
            position.ellipse_semi_minor_m = horizontal_uncertainty_m(bits.read(6)?);
            position.ellipse_angle_deg = Some(angle_deg(bits.read(8)?));
            read_altitude(bits, &mut position)?;
            position.altitude_accuracy_m = altitude_accuracy_from_code(bits.read(3)?);
            position.confidence_percent = confidence_percent(bits.read(3)?);
        }
        9 => {
            position.arc_inner_radius_m = Some(bits.read(16)? as f32 * 2.0);
            position.arc_outer_radius_m = Some(bits.read(16)? as f32 * 2.0);
            position.arc_start_deg = Some(angle_deg(bits.read(8)?));
            position.arc_stop_deg = Some(angle_deg(bits.read(8)?));
            position.confidence_percent = confidence_percent(bits.read(3)?);
        }
        10 => {
            let code = bits.read(3)? as u8;
            position.position_error_code = Some(code);
            position.position_error_m = position_error_m(u32::from(code));
        }
        _ => unreachable!(),
    }

    read_velocity_data(bits, &mut position)?;
    position.acknowledgement_requested = bits.read(1)? != 0;
    read_additional_data(bits, &mut position)?;
    Some(position)
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

    fn lip_sds(payload: &[u8]) -> SdsUserData {
        let mut bytes = vec![LIP_PROTOCOL_ID];
        bytes.extend(pack(payload));
        SdsUserData::Type4((8 + payload.len()) as u16, bytes)
    }

    #[test]
    fn decodes_complete_short_location_report() {
        let mut payload = Vec::new();
        push_bits(&mut payload, 0, 2); // short PDU
        push_bits(&mut payload, 1, 2); // position is less than five minutes old
        push_bits(&mut payload, angular_raw(18.6466, 25), 25);
        push_bits(&mut payload, angular_raw(54.3520, 24), 24);
        push_bits(&mut payload, 1, 3); // <20 m position error
        push_bits(&mut payload, 12, 7); // 12 km/h
        push_bits(&mut payload, 4, 4); // east
        push_bits(&mut payload, 0, 1); // reason for sending
        push_bits(&mut payload, 3, 8); // PTT detected

        let p = decode_tetra_lip_position(&lip_sds(&payload)).expect("position");

        assert!((p.lat - 54.3520).abs() < 0.0001);
        assert!((p.lon - 18.6466).abs() < 0.0001);
        assert_eq!(p.report_type, TetraLipReportType::Short);
        assert_eq!(p.location_shape, TetraLipLocationShape::PointWithPositionError);
        assert_eq!(p.time_elapsed, Some(1));
        assert_eq!(p.position_error_m, Some(20.0));
        assert_eq!(p.speed_kmh, Some(12.0));
        assert_eq!(p.heading_deg, Some(90.0));
        assert_eq!(p.reason_for_sending, Some(3));
    }

    #[test]
    fn decodes_long_ellipse_altitude_velocity_and_time() {
        let mut payload = Vec::new();
        push_bits(&mut payload, 1, 2); // extended PDU
        push_bits(&mut payload, 3, 4); // long location report
        push_bits(&mut payload, 2, 2); // absolute time of position
        push_bits(&mut payload, 15, 5);
        push_bits(&mut payload, 14, 5);
        push_bits(&mut payload, 7, 6);
        push_bits(&mut payload, 9, 6);
        push_bits(&mut payload, 8, 4); // ellipse + altitude + altitude uncertainty
        push_bits(&mut payload, angular_raw(18.6466, 25), 25);
        push_bits(&mut payload, angular_raw(54.3520, 24), 24);
        push_bits(&mut payload, 2, 6); // major axis <3.2 m
        push_bits(&mut payload, 1, 6); // minor axis <2 m
        push_bits(&mut payload, 64, 8); // 90 degree ellipse angle
        push_bits(&mut payload, 0, 1); // WGS84
        push_bits(&mut payload, 251, 11); // 50 m
        push_bits(&mut payload, 2, 3); // altitude uncertainty <5 m
        push_bits(&mut payload, 4, 3); // 95% confidence
        push_bits(&mut payload, 7, 3); // full velocity form
        push_bits(&mut payload, 30, 7); // 30.2 km/h
        push_bits(&mut payload, 1, 3); // <3 km/h uncertainty
        push_bits(&mut payload, 0, 1); // vertical direction up
        push_bits(&mut payload, 4, 7); // 4 km/h
        push_bits(&mut payload, 0, 3); // <1.5 km/h uncertainty
        push_bits(&mut payload, 128, 8); // south
        push_bits(&mut payload, 2, 3); // <6 degree uncertainty
        push_bits(&mut payload, 1, 1); // acknowledgement requested
        push_bits(&mut payload, 0, 1); // reason for sending
        push_bits(&mut payload, 20, 8); // user application initiated

        let p = decode_tetra_lip_position(&lip_sds(&payload)).expect("long position");

        assert_eq!(p.report_type, TetraLipReportType::Long);
        assert_eq!(p.location_shape, TetraLipLocationShape::EllipseWithAltitudeAndUncertainty);
        assert_eq!(
            p.time_of_position,
            Some(TetraLipTimeOfPosition {
                day: 15,
                hour: 14,
                minute: 7,
                second: 9,
            })
        );
        assert_eq!(p.altitude_m, Some(50));
        assert_eq!(p.altitude_reference, Some(TetraLipAltitudeReference::Wgs84));
        assert_eq!(p.altitude_accuracy_m, Some(5.0));
        assert_eq!(p.ellipse_angle_deg, Some(90.0));
        assert_eq!(p.confidence_percent, Some(95.0));
        assert!(p.speed_kmh.is_some_and(|v| (v - 30.2).abs() < 0.2));
        assert_eq!(p.vertical_speed_kmh, Some(4.0));
        assert_eq!(p.heading_deg, Some(180.0));
        assert!(p.acknowledgement_requested);
        assert_eq!(p.reason_for_sending, Some(20));
    }

    #[test]
    fn decodes_southern_and_western_coordinates() {
        let mut payload = Vec::new();
        push_bits(&mut payload, 0, 2);
        push_bits(&mut payload, 0, 2);
        push_bits(&mut payload, angular_raw(-58.3816, 25), 25);
        push_bits(&mut payload, angular_raw(-34.6037, 24), 24);
        push_bits(&mut payload, 0, 3);
        push_bits(&mut payload, 127, 7); // speed unknown
        push_bits(&mut payload, 15, 4);
        push_bits(&mut payload, 1, 1); // user-defined data
        push_bits(&mut payload, 0x42, 8);

        let p = decode_tetra_lip_position(&lip_sds(&payload)).expect("position");

        assert!((p.lat + 34.6037).abs() < 0.0001);
        assert!((p.lon + 58.3816).abs() < 0.0001);
        assert_eq!(p.speed_kmh, None, "ETSI code 127 means unknown, not a large speed");
        assert_eq!(p.user_defined_data, Some(0x42));
    }

    #[test]
    fn rejects_non_lip_sds() {
        let sds = SdsUserData::Type4(16, vec![0x82, 0x00]);
        assert!(decode_tetra_lip_position(&sds).is_none());
    }

    #[test]
    fn rejects_truncated_lip_without_panicking() {
        let sds = SdsUserData::Type4(24, vec![LIP_PROTOCOL_ID, 0x00, 0x00]);
        assert!(decode_tetra_lip_position(&sds).is_none());
    }
}
