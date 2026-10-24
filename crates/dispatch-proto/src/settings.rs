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
    /// Network-wide ban list: every node refuses these ISSIs and kicks them if registered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub banned: Vec<Issi>,
}

/// Upper bound on the ban list (keeps the settings message small).
pub const MAX_BANNED: usize = 10_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("ISSI 0 is not a subscriber address")]
    IssiZero,
    #[error("ISSI 16777215 (all ones) is the reserved broadcast address (EN 300 392-1 §7.7.8)")]
    IssiBroadcast,
    #[error("ISSI {0} does not fit in 24 bits (max 16777214)")]
    IssiTooLarge(u32),
    #[error("the ban list is limited to 10000 ISSIs")]
    TooManyBanned,
    #[error("the dispatcher ISSI cannot be banned")]
    DispatcherBanned,
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
        if let Some(issi) = self.dispatcher_issi {
            validate_issi(issi)?;
            if self.banned.contains(&issi) {
                return Err(SettingsError::DispatcherBanned);
            }
        }
        if self.banned.len() > MAX_BANNED {
            return Err(SettingsError::TooManyBanned);
        }
        self.banned.iter().try_for_each(|&i| validate_issi(i))
    }

    /// Sorted, without duplicates (stable for comparisons and storage).
    pub fn normalized(mut self) -> Self {
        self.banned.sort_unstable();
        self.banned.dedup();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_rules() {
        let ok = DispatchSettings { dispatcher_issi: Some(9_000_001), banned: vec![2_601_292] };
        assert_eq!(ok.validate(), Ok(()));
        let own = DispatchSettings { dispatcher_issi: Some(9_000_001), banned: vec![9_000_001] };
        assert_eq!(own.validate(), Err(SettingsError::DispatcherBanned));
        let bad = DispatchSettings { dispatcher_issi: None, banned: vec![0xFF_FFFF] };
        assert_eq!(bad.validate(), Err(SettingsError::IssiBroadcast));
    }

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
        let s = DispatchSettings { dispatcher_issi: Some(9_000_001), banned: vec![] };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"dispatcher_issi":9000001}"#);
        let s = DispatchSettings { dispatcher_issi: None, banned: vec![7, 3, 7] }.normalized();
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"dispatcher_issi":null,"banned":[3,7]}"#);
        assert_eq!(serde_json::from_str::<DispatchSettings>("{}").unwrap(), DispatchSettings::default());
    }
}
