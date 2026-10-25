//! Routing rules for the dispatcher link. One place for "dispatch groups never go to Brew".
//!
//! Three layers keep dispatcher traffic away from Brew/TetraPACK/BrandMeister:
//! 1. [`validate_against_brew`] at startup — the link refuses to start if any dispatch group is
//!    Brew-routable.
//! 2. [`gate_call`] on every dispatcher call request — the config can be edited live.
//! 3. [`suppress_brew_for_gssi`], consulted by CMCE for **every** Brew notification
//!    (`BrewNotification::enabled`): a dispatch group is never announced to Brew, whoever
//!    started the call — dispatcher, a local radio, or a radio answering the dispatcher.

use tetra_config::bluestation::SharedConfig;
use tetra_saps::control::call_control::NetworkCallRejectReason;

use crate::net_brew as brew;

/// The dispatcher link is configured and enabled.
pub fn is_enabled(config: &SharedConfig) -> bool {
    config.config().dispatcher.as_ref().is_some_and(|d| d.enabled)
}

/// `gssi` is one of the dispatcher's groups (and the link is enabled): listed in `[dispatcher]`,
/// or — with `local_groups` — any group Brew never carries.
pub fn is_dispatch_gssi(config: &SharedConfig, gssi: u32) -> bool {
    let Some(d) = config.config().dispatcher.clone() else { return false };
    d.allows_gssi(gssi) || (d.enabled && d.local_groups && (1..=0xFF_FFFE).contains(&gssi) && !brew_would_route(config, gssi))
}

/// Ranges of groups the dispatcher may use (reported to the core, protocol v4).
pub fn dispatch_ranges(config: &SharedConfig) -> Vec<(u32, u32)> {
    let Some(d) = config.config().dispatcher.clone() else { return Vec::new() };
    if !d.enabled {
        return Vec::new();
    }
    let mut out: Vec<(u32, u32)> = d.gssis.iter().map(|g| (*g, *g)).collect();
    if d.local_groups {
        if config.config().brew.is_none() {
            out.push((1, 0xFF_FFFE));
        } else {
            for r in config.config().cell.local_ssi_ranges.as_slice() {
                let (start, end) = (r.start.max(1), r.end.min(0xFF_FFFE));
                if start <= end {
                    out.push((start, end));
                }
            }
        }
    }
    out
}

/// Brew would carry traffic for this group in either direction.
pub fn brew_would_route(config: &SharedConfig, gssi: u32) -> bool {
    brew::is_brew_gssi_routable(config, gssi) || brew::is_brew_inbound_allowed(config, gssi)
}

/// Layer 3: CMCE must not tell Brew about calls on a dispatch group.
pub fn suppress_brew_for_gssi(config: &SharedConfig, gssi: u32) -> bool {
    is_dispatch_gssi(config, gssi)
}

/// Layer 1: every dispatch group must be local-only. Returns the offending groups.
pub fn validate_against_brew(config: &SharedConfig) -> Result<(), Vec<u32>> {
    let Some(dispatcher) = config.config().dispatcher.clone() else {
        return Ok(());
    };
    let conflicts: Vec<u32> = dispatcher.gssis.iter().copied().filter(|g| brew_would_route(config, *g)).collect();
    if conflicts.is_empty() { Ok(()) } else { Err(conflicts) }
}

/// Layer 2: may the dispatcher start a call on `gssi` right now?
pub fn gate_call(config: &SharedConfig, gssi: u32) -> Result<(), NetworkCallRejectReason> {
    if !is_dispatch_gssi(config, gssi) {
        return Err(NetworkCallRejectReason::NotAllowed);
    }
    if brew_would_route(config, gssi) {
        tracing::error!(
            "Dispatcher: GSSI {} became Brew-routable at runtime; refusing the dispatcher call",
            gssi
        );
        return Err(NetworkCallRejectReason::BrewRoutable);
    }
    Ok(())
}
