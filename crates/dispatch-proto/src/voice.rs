//! Voice frames. One TETRA speech frame = 274 ACELP bits = 60 ms, packed MSB-first into
//! 35 bytes (last 6 bits zero) — the same packing FlowStation's Brew entity uses, so frames
//! recorded from monitoring can be replayed without conversion.

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::{CallId, Gssi, Issi};

pub const ACELP_FRAME_BYTES: usize = 35;
pub const ACELP_FRAME_BITS: usize = 274;

/// In JSON a frame is a 70-character hex string (serde has no impls for `[u8; 35]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct AcelpFrame(pub [u8; ACELP_FRAME_BYTES]);

impl Serialize for AcelpFrame {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let hex: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for AcelpFrame {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(d)?;
        if hex.len() != ACELP_FRAME_BYTES * 2 || !hex.is_ascii() {
            return Err(serde::de::Error::custom("ACELP frame must be 70 hex characters"));
        }
        let mut out = [0u8; ACELP_FRAME_BYTES];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(serde::de::Error::custom)?;
        }
        Ok(Self(out))
    }
}

impl AcelpFrame {
    /// Pack 274 one-bit-per-byte values (as FlowStation's UMAC delivers them).
    pub fn from_bits(bits: &[u8]) -> Option<Self> {
        if bits.len() < ACELP_FRAME_BITS {
            return None;
        }
        let mut out = [0u8; ACELP_FRAME_BYTES];
        for (i, bit) in bits.iter().take(ACELP_FRAME_BITS).enumerate() {
            if *bit & 1 == 1 {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        Some(Self(out))
    }

    /// Unpack into 274 one-bit-per-byte values.
    pub fn to_bits(&self) -> Vec<u8> {
        (0..ACELP_FRAME_BITS).map(|i| (self.0[i / 8] >> (7 - i % 8)) & 1).collect()
    }
}

/// Voice on the node link (QUIC datagrams) and on the bus.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub enum VoiceDatagram {
    /// Dispatcher → node → air.
    Dl { call_id: CallId, seq: u32, frame: AcelpFrame },
    /// Air → node → core (monitoring, recording).
    Ul { call_id: CallId, gssi: Gssi, speaker_issi: Issi, seq: u32, frame: AcelpFrame },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_packing_roundtrip() {
        let bits: Vec<u8> = (0..ACELP_FRAME_BITS).map(|i| ((i * 7 + 3) % 2) as u8).collect();
        let frame = AcelpFrame::from_bits(&bits).unwrap();
        assert_eq!(frame.to_bits(), bits);
        assert_eq!(frame.0[34] & 0b0011_1111, 0, "6 padding bits stay zero");
    }

    #[test]
    fn json_hex_roundtrip() {
        let frame = AcelpFrame([0x0f; ACELP_FRAME_BYTES]);
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(json.len(), 72);
        assert_eq!(serde_json::from_str::<AcelpFrame>(&json).unwrap(), frame);
        assert!(serde_json::from_str::<AcelpFrame>("\"abcd\"").is_err());
    }

    #[test]
    fn short_input_is_rejected() {
        assert!(AcelpFrame::from_bits(&[1; 100]).is_none());
    }

    #[test]
    fn datagram_roundtrip_and_size() {
        let d = VoiceDatagram::Ul {
            call_id: 3,
            gssi: 91,
            speaker_issi: 2_603_877,
            seq: 42,
            frame: AcelpFrame([0xAA; ACELP_FRAME_BYTES]),
        };
        let bytes = crate::encode(&d);
        assert!(bytes.len() < 64, "fits easily in one datagram: {} bytes", bytes.len());
        assert_eq!(crate::decode::<VoiceDatagram>(&bytes).unwrap(), d);
    }
}
