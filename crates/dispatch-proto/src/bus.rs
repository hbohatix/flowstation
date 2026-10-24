//! NATS subjects and envelopes used between platform services.
//!
//! | Subject | Payload | Direction |
//! |---|---|---|
//! | `fs.node.<node>.events` | JSON [`Envelope`] of [`NodeToCore`](crate::NodeToCore) or [`CoreEvent`] | core → services |
//! | `fs.node.<node>.cmd` | JSON [`Envelope`] of [`CoreToNode`](crate::CoreToNode) | services → core → node |
//! | `fs.voice.<node>.<call_id>` | bitcode [`VoiceDatagram::Ul`](crate::VoiceDatagram) | core → services |
//! | `fs.voicedl.<node>.<call_id>` | bitcode [`VoiceDatagram::Dl`](crate::VoiceDatagram) | services → core → node |
//! | `fs.core.nodes` | request/reply: JSON list of [`Envelope`] of [`CoreEvent::NodeOnline`] | services → core |

use serde::{Deserialize, Serialize};

use crate::{CallId, Gssi, Issi};

pub const ALL_NODE_EVENTS: &str = "fs.node.*.events";
pub const ALL_NODE_COMMANDS: &str = "fs.node.*.cmd";
pub const ALL_VOICE: &str = "fs.voice.>";
pub const ALL_VOICE_DL: &str = "fs.voicedl.>";
/// Request/reply: the core answers with the nodes connected right now (for services that start late).
pub const CORE_NODES: &str = "fs.core.nodes";

pub fn node_events(node_id: &str) -> String {
    format!("fs.node.{}.events", token(node_id))
}

pub fn node_commands(node_id: &str) -> String {
    format!("fs.node.{}.cmd", token(node_id))
}

pub fn voice(node_id: &str, call_id: CallId) -> String {
    format!("fs.voice.{}.{}", token(node_id), call_id)
}

/// Downlink voice for every call on one node: `fs.voicedl.<node>.*`.
pub fn voice_dl_for_node(node_id: &str) -> String {
    format!("fs.voicedl.{}.*", token(node_id))
}

pub fn voice_dl(node_id: &str, call_id: CallId) -> String {
    format!("fs.voicedl.{}.{}", token(node_id), call_id)
}

/// Parse `fs.voice.<node>.<call_id>`.
pub fn parse_voice_subject(subject: &str) -> Option<(&str, CallId)> {
    let mut parts = subject.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("fs"), Some("voice"), Some(node), Some(call), None) => Some((node, call.parse().ok()?)),
        _ => None,
    }
}

/// NATS subject tokens cannot contain `.`, `*`, `>` or whitespace.
pub fn token(node_id: &str) -> String {
    node_id
        .chars()
        .map(|c| if c == '.' || c == '*' || c == '>' || c.is_whitespace() { '_' } else { c })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub node_id: String,
    /// Unix time in milliseconds when the core received/emitted the message.
    pub at_ms: u64,
    pub body: T,
}

/// Events the core itself emits about a node (published on `fs.node.<node>.events`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CoreEvent {
    /// `dispatcher_issi` is the node's own fallback; the effective value follows from
    /// the platform setting and the node's `SettingsApplied` reply.
    NodeOnline { software: String, dispatcher_issi: Option<Issi>, dispatch_gssis: Vec<Gssi> },
    NodeOffline { reason: String },
    /// Link quality, every few seconds while connected: QUIC round-trip time in ms.
    NodeLink { rtt_ms: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects() {
        assert_eq!(node_events("gdansk-01"), "fs.node.gdansk-01.events");
        assert_eq!(node_events("bad.id *"), "fs.node.bad_id__.events");
        assert_eq!(voice("gdansk-01", 218), "fs.voice.gdansk-01.218");
        assert_eq!(voice_dl("gdansk-01", 218), "fs.voicedl.gdansk-01.218");
        assert_eq!(voice_dl_for_node("gdansk-01"), "fs.voicedl.gdansk-01.*");
        assert_eq!(parse_voice_subject("fs.voice.gdansk-01.218"), Some(("gdansk-01", 218)));
        assert_eq!(parse_voice_subject("fs.voice.x"), None);
        assert_eq!(parse_voice_subject("fs.voicedl.x.1"), None);
        assert_eq!(parse_voice_subject("fs.voice.x.notanumber"), None);
    }

    #[test]
    fn core_event_json_matches_web_shape() {
        let json = serde_json::to_string(&CoreEvent::NodeOffline { reason: "timeout".into() }).unwrap();
        assert_eq!(json, r#"{"NodeOffline":{"reason":"timeout"}}"#);
    }
}
