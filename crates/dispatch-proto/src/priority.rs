//! Call priority, ETSI EN 300 392-2 V3.8.1 §14.8.12, table 14.46 (4 bits):
//! 0 = not defined, 1..=11 = priority 1..11, 12..=14 = pre-emptive 1..3,
//! 15 = pre-emptive 4 (Emergency). Used by SS-PC (EN 300 392-12-10) and SS-PPC (EN 300 392-12-16).
//!
//! JSON decoding validates the range. Binary (bitcode) decoding does not, so a receiver must
//! re-check with [`CallPriority::new`] (or [`CallPriority::checked`]) at the trust boundary.

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct CallPriority(u8);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("call priority {0} is out of range 0..=15")]
pub struct PriorityError(pub u8);

impl CallPriority {
    pub const NOT_DEFINED: Self = Self(0);
    pub const HIGHEST_NORMAL: Self = Self(11);
    pub const PREEMPTIVE_MIN: Self = Self(12);
    pub const EMERGENCY: Self = Self(15);

    pub fn new(value: u8) -> Result<Self, PriorityError> {
        if value <= 15 { Ok(Self(value)) } else { Err(PriorityError(value)) }
    }

    pub fn value(self) -> u8 {
        self.0
    }

    /// Re-validate a value that came in over the binary link.
    pub fn checked(self) -> Result<Self, PriorityError> {
        Self::new(self.0)
    }

    /// Pre-emptive priority 1..4 (values 12..=15).
    pub fn is_preemptive(self) -> bool {
        self >= Self::PREEMPTIVE_MIN
    }

    pub fn is_emergency(self) -> bool {
        self == Self::EMERGENCY
    }
}

impl TryFrom<u8> for CallPriority {
    type Error = PriorityError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CallPriority> for u8 {
    fn from(p: CallPriority) -> u8 {
        p.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_14_46_boundaries() {
        assert!(!CallPriority::NOT_DEFINED.is_preemptive());
        assert!(!CallPriority::new(11).unwrap().is_preemptive());
        assert!(CallPriority::new(12).unwrap().is_preemptive());
        assert!(CallPriority::new(15).unwrap().is_emergency());
        assert_eq!(CallPriority::new(16), Err(PriorityError(16)));
    }

    #[test]
    fn binary_decode_must_be_rechecked() {
        let forged = crate::encode(&200u8);
        let p: CallPriority = crate::decode(&forged).unwrap();
        assert_eq!(p.checked(), Err(PriorityError(200)));
    }

    #[test]
    fn json_rejects_out_of_range() {
        assert!(serde_json::from_str::<CallPriority>("16").is_err());
        assert_eq!(serde_json::from_str::<CallPriority>("12").unwrap(), CallPriority::PREEMPTIVE_MIN);
    }
}
