//! Platform settings that operators change at runtime (console → dispatch → NATS KV → core → nodes).

use serde::{Deserialize, Serialize};

use crate::Issi;

/// NATS JetStream key-value bucket holding the settings.
pub const SETTINGS_BUCKET: &str = "fs-settings";
/// Key of [`DispatchSettings`] in [`SETTINGS_BUCKET`].
pub const DISPATCH_KEY: &str = "dispatch";

/// Largest valid SSI: 24 bits, minus the all-ones broadcast address.
pub const SSI_MAX: u32 = 0xFF_FFFE;
/// All-ones SSI: reserved broadcast group "all MSs" (EN 300 392-1 V1.6.1 §7.7.8).
pub const SSI_BROADCAST: u32 = 0xFF_FFFF;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSettings {
    /// ISSI radios see as the calling party of dispatcher calls.
    /// `None` = use each node's own configured fallback.
    pub dispatcher_issi: Option<Issi>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("ISSI 0 is not a subscriber address")]
    IssiZero,
    #[error("ISSI 16777215 (all ones) is the reserved broadcast address (EN 300 392-1 §7.7.8)")]
    IssiBroadcast,
    #[error("ISSI {0} does not fit in 24 bits (max 16777214)")]
    IssiTooLarge(u32),
}

/// Valid individual SSI for a dispatcher: 1..=16777214.
pub fn validate_issi(issi: Issi) -> Result<(), SettingsError> {
    match issi {
        0 => Err(SettingsError::IssiZero),
        SSI_BROADCAST => Err(SettingsError::IssiBroadcast),
        v if v > SSI_MAX => Err(SettingsError::IssiTooLarge(v)),
        _ => Ok(()),
    }
}

impl DispatchSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        match self.dispatcher_issi {
            Some(issi) => validate_issi(issi),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issi_rules() {
        assert_eq!(validate_issi(0), Err(SettingsError::IssiZero));
        assert_eq!(validate_issi(1), Ok(()));
        assert_eq!(validate_issi(9_000_001), Ok(()));
        assert_eq!(validate_issi(0xFF_FFFE), Ok(()));
        assert_eq!(validate_issi(0xFF_FFFF), Err(SettingsError::IssiBroadcast));
        assert_eq!(validate_issi(0x100_0000), Err(SettingsError::IssiTooLarge(0x100_0000)));
    }

    #[test]
    fn json_shape() {
        let s = DispatchSettings { dispatcher_issi: Some(9_000_001) };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"dispatcher_issi":9000001}"#);
        assert_eq!(serde_json::from_str::<DispatchSettings>("{}").unwrap(), DispatchSettings::default());
    }
}
