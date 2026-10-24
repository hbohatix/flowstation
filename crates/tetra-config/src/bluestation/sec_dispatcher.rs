//! `[dispatcher]` — link to the flowstation-platform core (dispatcher console).
//!
//! ```toml
//! [dispatcher]
//! server = "192.168.1.10:4433"            # core QUIC address (host:port)
//! server_name = "core"                    # TLS name in the core certificate (default: host)
//! ca_cert = "/etc/flowstation/core-cert.pem"  # pin the core's certificate (PEM); omit = system CAs
//! node_id = "gdansk-01"
//! token = "secret"                        # must match FS_NODE_TOKENS on the core
//! gssis = [91, 401]                       # groups the dispatcher may call; never Brew-routable
//! fallback_issi = 9000001                 # used when the console setting is empty
//! default_priority = 0                    # EN 300 392-2 table 14.46 (0 = not defined)
//! max_tx_seconds = 60                     # cut a dispatcher transmission after this long
//! link_timeout_ms = 6000                  # end dispatcher TX this soon after the link dies
//! ```
//!
//! Dispatcher traffic is local to this cell. Startup fails if any of `gssis` is routable to Brew.

use std::collections::HashMap;

use serde::Deserialize;
use toml::Value;

/// Largest individual SSI; 0xFFFFFF (all ones) is the broadcast address (EN 300 392-1 §7.7.8).
pub const DISPATCH_SSI_MAX: u32 = 0xFF_FFFE;

#[derive(Debug, Clone, PartialEq)]
pub struct CfgDispatcher {
    pub enabled: bool,
    pub server_host: String,
    pub server_port: u16,
    pub server_name: Option<String>,
    pub ca_cert: Option<String>,
    pub node_id: String,
    pub token: String,
    pub gssis: Vec<u32>,
    pub fallback_issi: Option<u32>,
    pub default_priority: u8,
    pub max_tx_seconds: u32,
    pub link_timeout_ms: u32,
}

#[derive(Deserialize)]
pub struct CfgDispatcherDto {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub server: String,
    pub server_name: Option<String>,
    pub ca_cert: Option<String>,
    pub node_id: String,
    pub token: String,
    pub gssis: Vec<u32>,
    pub fallback_issi: Option<u32>,
    #[serde(default)]
    pub default_priority: u8,
    #[serde(default = "default_max_tx")]
    pub max_tx_seconds: u32,
    #[serde(default = "default_link_timeout")]
    pub link_timeout_ms: u32,

    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

fn default_true() -> bool {
    true
}
fn default_max_tx() -> u32 {
    60
}
fn default_link_timeout() -> u32 {
    6000
}

fn valid_ssi(v: u32) -> bool {
    (1..=DISPATCH_SSI_MAX).contains(&v)
}

pub fn apply_dispatcher_patch(src: CfgDispatcherDto) -> Result<CfgDispatcher, String> {
    let (host, port) = src
        .server
        .rsplit_once(':')
        .ok_or_else(|| format!("dispatcher: server must be host:port, got {:?}", src.server))?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port: u16 = port.parse().map_err(|_| format!("dispatcher: bad port in server {:?}", src.server))?;
    if host.is_empty() {
        return Err("dispatcher: server host is empty".to_string());
    }
    if src.node_id.trim().is_empty() {
        return Err("dispatcher: node_id must not be empty".to_string());
    }
    if src.token.is_empty() {
        return Err("dispatcher: token must not be empty".to_string());
    }
    if src.gssis.is_empty() {
        return Err("dispatcher: gssis must list at least one group".to_string());
    }
    if let Some(bad) = src.gssis.iter().find(|g| !valid_ssi(**g)) {
        return Err(format!("dispatcher: gssi {bad} is not a valid group SSI (1..=16777214)"));
    }
    if let Some(issi) = src.fallback_issi
        && !valid_ssi(issi)
    {
        return Err(format!("dispatcher: fallback_issi {issi} is not a valid ISSI (1..=16777214)"));
    }
    if src.default_priority > 15 {
        return Err(format!("dispatcher: default_priority {} is out of range 0..=15", src.default_priority));
    }
    if !(5..=600).contains(&src.max_tx_seconds) {
        return Err(format!("dispatcher: max_tx_seconds {} must be 5..=600", src.max_tx_seconds));
    }
    if !(1000..=60_000).contains(&src.link_timeout_ms) {
        return Err(format!("dispatcher: link_timeout_ms {} must be 1000..=60000", src.link_timeout_ms));
    }
    let mut gssis = src.gssis;
    gssis.sort_unstable();
    gssis.dedup();

    Ok(CfgDispatcher {
        enabled: src.enabled,
        server_host: host,
        server_port: port,
        server_name: src.server_name,
        ca_cert: src.ca_cert,
        node_id: src.node_id,
        token: src.token,
        gssis,
        fallback_issi: src.fallback_issi,
        default_priority: src.default_priority,
        max_tx_seconds: src.max_tx_seconds,
        link_timeout_ms: src.link_timeout_ms,
    })
}

impl CfgDispatcher {
    pub fn allows_gssi(&self, gssi: u32) -> bool {
        self.enabled && self.gssis.binary_search(&gssi).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(toml_src: &str) -> CfgDispatcherDto {
        toml::from_str(toml_src).unwrap()
    }

    const BASE: &str = r#"
server = "192.168.1.10:4433"
node_id = "gdansk-01"
token = "s"
gssis = [401, 91, 91]
"#;

    #[test]
    fn minimal_section_uses_safe_defaults() {
        let cfg = apply_dispatcher_patch(dto(BASE)).unwrap();
        assert!(cfg.enabled);
        assert_eq!((cfg.server_host.as_str(), cfg.server_port), ("192.168.1.10", 4433));
        assert_eq!(cfg.gssis, vec![91, 401], "sorted and deduplicated");
        assert_eq!(cfg.default_priority, 0);
        assert_eq!(cfg.max_tx_seconds, 60);
        assert_eq!(cfg.link_timeout_ms, 6000);
        assert!(cfg.allows_gssi(91) && !cfg.allows_gssi(92));
    }

    #[test]
    fn rejects_invalid_values() {
        let bad = |extra: &str| apply_dispatcher_patch(dto(&format!("{BASE}{extra}"))).unwrap_err();
        assert!(bad("fallback_issi = 16777215").contains("fallback_issi"));
        assert!(bad("default_priority = 16").contains("default_priority"));
        assert!(bad("max_tx_seconds = 1").contains("max_tx_seconds"));
        let no_port = r#"server = "core"
node_id = "n"
token = "s"
gssis = [91]"#;
        assert!(apply_dispatcher_patch(dto(no_port)).unwrap_err().contains("host:port"));
        let broadcast_group = r#"server = "core:4433"
node_id = "n"
token = "s"
gssis = [16777215]"#;
        assert!(apply_dispatcher_patch(dto(broadcast_group)).unwrap_err().contains("gssi"));
        let no_groups = r#"server = "core:4433"
node_id = "n"
token = "s"
gssis = []"#;
        assert!(apply_dispatcher_patch(dto(no_groups)).unwrap_err().contains("at least one"));
    }

    #[test]
    fn disabled_section_allows_nothing() {
        let cfg = apply_dispatcher_patch(dto(&format!("{BASE}enabled = false"))).unwrap();
        assert!(!cfg.allows_gssi(91));
    }
}
