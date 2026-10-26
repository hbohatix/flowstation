//! COPY of hbohatix/flowstation-platform `crates/proto` (protocol v5) — do not edit here; change
//! the protocol in flowstation-platform and re-copy so both sides stay byte-compatible.
//!
//! Wire protocol `flowstation-dispatch-v0`.
//!
//! Two links use these types:
//! - **Node link**: a FlowStation base station connects out to the core over QUIC/TLS.
//!   Control messages go on the reliable stream ([`NodeToCore`], [`CoreToNode`]),
//!   voice goes in datagrams ([`VoiceDatagram`]).
//! - **Bus**: platform services talk over NATS; see [`bus`].
//!
//! Brew rule: nothing in this protocol forwards traffic to Brew/TetraPack. Dispatcher calls are
//! local to the node. A node only lists groups that are *not* routable to Brew in
//! [`NodeToCore::Hello::dispatch_gssis`], and enforces that itself at startup.

pub mod bus;
mod codec;
pub mod framing;
mod priority;
pub mod sds;
pub mod settings;
mod voice;

pub use codec::{CodecError, decode, encode};
pub use priority::{CallPriority, PriorityError};
pub use settings::DispatchSettings;
pub use voice::{ACELP_FRAME_BYTES, AcelpFrame, VoiceDatagram};

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Protocol version carried in [`NodeToCore::Hello`]. Bump on any incompatible change.
/// v3 adds [`NodeToCore::BackhaulStatus`], v4 [`NodeToCore::StationInfo`], v5 location reports
/// with reason, statuses, emergency end reasons and [`CoreToNode::SendStatus`] — always at the
/// end of the enums, so older nodes still work.
pub const PROTOCOL_VERSION: u16 = 5;

/// Oldest node protocol the core accepts (later versions only append messages).
pub const MIN_PROTOCOL_VERSION: u16 = 2;

/// ALPN identifier for the QUIC node link.
pub const ALPN: &[u8] = b"flowstation-dispatch-v0";

/// ALPN FlowStation's `QuicTransport` sends today (hard-coded). The core accepts it too,
/// so the fork can use its existing transport unchanged.
pub const ALPN_FLOWSTATION_LEGACY: &[u8] = b"hq-29";

/// Request id chosen by the requester (core/console) to match replies.
pub type ReqId = u32;
/// CMCE call identifier allocated by the node (EN 300 392-2 §14.8.3, table 14.36, 14 bits).
pub type CallId = u16;
/// Individual short subscriber identity (24 bits).
pub type Issi = u32;
/// Group short subscriber identity (24 bits).
pub type Gssi = u32;

/// Why a group transmission request was refused by the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub enum RejectReason {
    /// GSSI is not on the node's dispatcher list.
    NotAllowed,
    /// GSSI is routable to Brew; dispatcher traffic must stay local.
    BrewRoutable,
    /// No registered radio listens to this group.
    NoListeners,
    /// A radio holds the floor and the request cannot interrupt it.
    FloorBusy,
    /// No free traffic timeslot.
    NoResources,
    /// The node link went down before the call was set up.
    LinkDown,
    /// No dispatcher ISSI: neither set in the platform settings nor in the node config.
    NoDispatcherIssi,
}

/// Why the node stopped a dispatcher transmission on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub enum StopReason {
    /// Dispatcher asked to stop (PTT released).
    Requested,
    /// Transmission exceeded the node's maximum TX time.
    MaxTxTime,
    /// The node lost the link (heartbeat timeout).
    LinkLost,
    /// The call was released (timeout, pre-emption, no listeners left).
    CallReleased,
}

/// What a console operator asks a node to do with one radio (protocol v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioAction {
    /// Force deregistration (the radio has to register again).
    Kick,
    /// D-LOCATION UPDATE COMMAND with group report request (EN 300 392-2 §16.4).
    ReRegister,
    /// LIP IMMEDIATE LOCATION REPORT REQUEST (TS 100 392-18-1 §6.2.16).
    RequestPosition,
}

/// One carrier of a base station.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct CarrierInfo {
    pub number: u16,
    pub dl_hz: u64,
    pub ul_hz: u64,
}

/// Periodic health and load report of a node (protocol v2).
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Seconds since the station process started.
    pub uptime_s: u64,
    pub carriers: Vec<CarrierInfo>,
    pub mcc: u16,
    pub mnc: u16,
    pub location_area: u16,
    /// Radios registered on this station right now.
    pub registered: u32,
    /// Group calls in progress.
    pub active_calls: u32,
    /// Traffic timeslots the station can use.
    pub timeslots_total: u32,
    pub cpu_temp_c: Option<f32>,
}

/// A neighbour cell the station announces (EN 300 392-2 §18.5.17, D-NWRK-BROADCAST).
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct NeighbourCell {
    /// Cell identifier within the cluster (0–31).
    pub cell_id: u8,
    pub main_carrier: u16,
    pub location_area: Option<u16>,
    pub mcc: Option<u16>,
    pub mnc: Option<u16>,
    pub synchronized: bool,
}

/// Inclusive range of group SSIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub struct GssiRange {
    pub start: u32,
    pub end: u32,
}

impl GssiRange {
    pub fn contains(&self, gssi: u32) -> bool {
        (self.start..=self.end).contains(&gssi)
    }
}

/// Static identity of a station (protocol v4), sent with every [`NodeToCore::NodeStatus`].
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct StationInfo {
    /// Repeater ID the station uses on Brew / TetraPack (the `[brew]` username).
    pub repeater_id: Option<u32>,
    /// Brew server host the station connects to.
    pub brew_host: Option<String>,
    pub colour_code: u8,
    pub neighbours: Vec<NeighbourCell>,
    /// Groups the dispatcher may transmit on: the `[dispatcher]` groups plus, with
    /// `local_groups`, every local-only group (never Brew-routable). The core routes dispatcher
    /// PTT by this; the station checks it again for every call.
    pub dispatch_ranges: Vec<GssiRange>,
}

/// A valid position from a LIP report (TS 100 392-18-1 §6.2.1 / §6.2.2).
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct GeoFix {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f32>,
    pub speed_kmh: Option<f32>,
    pub heading_deg: Option<f32>,
    pub altitude_m: Option<i32>,
}

/// Every LIP location report a radio sends (protocol v5). `fix: None` = the radio could not
/// determine its position (position error "not known", no shape, or 0/0) — it is still a
/// report, with its reason.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub struct LocationReport {
    pub issi: Issi,
    pub fix: Option<GeoFix>,
    /// Reason for sending (TS 100 392-18-1 §6.3.64, table 6.94): 0 power on, 2 emergency,
    /// 20 user application, 32 response to an immediate request, 129 max interval, …
    pub reason: Option<u8>,
    /// Long location report (else short).
    pub long: bool,
}

/// Why an emergency ended on the station (protocol v5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub enum EmergencyEnd {
    /// The radio sent a normal status after the emergency.
    Radio,
    /// No more emergency statuses for the station's clear timeout — the radio did NOT cancel.
    Timeout,
    /// Cleared on the station (its dashboard).
    Operator,
}

/// Messages a FlowStation node sends to the core on the reliable stream.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub enum NodeToCore {
    /// First message after connecting.
    Hello {
        protocol_version: u16,
        node_id: String,
        software: String,
        token: String,
        /// Node's own fallback dispatcher ISSI (from its config). The platform setting
        /// ([`CoreToNode::Settings`]) takes precedence when set.
        dispatcher_issi: Option<Issi>,
        /// Groups the dispatcher may use on this node. Never Brew-routable.
        dispatch_gssis: Vec<Gssi>,
    },
    Heartbeat { seq: u32 },
    GroupTxGranted { req_id: ReqId, call_id: CallId, carrier: u16, ts: u8 },
    GroupTxRejected { req_id: ReqId, reason: RejectReason },
    GroupCallStarted { call_id: CallId, gssi: Gssi, caller_issi: Issi, priority: CallPriority },
    GroupCallEnded { call_id: CallId, gssi: Gssi },
    SpeakerChanged { call_id: CallId, gssi: Gssi, speaker_issi: Issi },
    TxForcedStop { call_id: CallId, reason: StopReason },
    MsRegistered { issi: Issi },
    MsDeregistered { issi: Issi },
    MsGroups { issi: Issi, gssis: Vec<Gssi> },
    Emergency { issi: Issi, gssi: Gssi },
    EmergencyCleared { issi: Issi },
    /// Reply to [`CoreToNode::Settings`]: the dispatcher ISSI the node now uses, or why it
    /// refused the new value (e.g. a registered radio already has that ISSI).
    /// `banned` = how many ISSIs of the ban list the node enforces (v2).
    SettingsApplied { dispatcher_issi: Option<Issi>, error: Option<String>, banned: u32 },
    // ----- v1 -----
    /// Answer to [`CoreToNode::Dgna`] / [`CoreToNode::SendSds`]: accepted for sending or refused.
    CommandResult { req_id: ReqId, ok: bool, detail: Option<String> },
    /// SS-DGNA outcome reported by the radio / MM (EN 300 392-12-22).
    DgnaStatus { issi: Issi, gssi: Gssi, attach: bool, accepted: bool, detail: String },
    /// A text SDS seen on the cell (uplink from a radio, or sent by the node).
    SdsReceived { source_issi: Issi, dest_ssi: u32, is_group: bool, text: String },
    /// Decoded LIP position (TS 100 392-18-1).
    MsPosition {
        issi: Issi,
        lat: f64,
        lon: f64,
        accuracy_m: Option<f32>,
        speed_kmh: Option<f32>,
        heading_deg: Option<f32>,
    },
    /// Uplink signal level of a radio, rate-limited by the node.
    MsRssi { issi: Issi, rssi_dbfs: f32 },
    // ----- v2 -----
    /// Sent after Hello and every 10 s.
    NodeStatus(NodeStatus),
    // ----- v3 -----
    /// State of the node's network backhaul (Brew / TetraPACK), sent with every
    /// [`NodeToCore::NodeStatus`]. `brew_configured: false` = the node has no `[brew]` section.
    BackhaulStatus { brew_configured: bool, brew_connected: bool },
    // ----- v4 -----
    /// Repeater ID, Brew server, colour code and neighbour cells, with every `NodeStatus`.
    StationInfo(StationInfo),
    // ----- v5 -----
    /// Every LIP report, with its reason and whether it carries a position.
    MsLocation(LocationReport),
    /// A pre-coded status (U-STATUS, EN 300 392-2 §14.8.34) sent by a radio: 0 = emergency,
    /// 32768–65535 network / user specific. SDS-TL short reports are not included.
    StatusReceived { source_issi: Issi, dest_ssi: u32, dest_is_group: bool, status: u16 },
    /// The emergency of a radio ended on the station, and why.
    EmergencyEnded { issi: Issi, by: EmergencyEnd },
}

/// Messages the core sends to a node on the reliable stream.
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize)]
pub enum CoreToNode {
    HelloAck { protocol_version: u16, accepted: bool, reason: Option<String> },
    Heartbeat { seq: u32 },
    /// Start a dispatcher transmission on a group (new call, or speaker change in hangtime).
    GroupTxRequest { req_id: ReqId, gssi: Gssi, priority: CallPriority },
    /// Dispatcher released PTT: the call goes to hangtime so radios can answer.
    GroupTxStop { call_id: CallId },
    /// Groups whose voice the node should send as [`VoiceDatagram::Ul`]; `all` (v2) = every
    /// group call on the cell, local and network (Brew) speakers alike. Receive only.
    MonitorSet { all: bool, gssis: Vec<Gssi> },
    /// Platform settings. Sent right after [`CoreToNode::HelloAck`] and whenever they change.
    /// `dispatcher_issi: None` = use the node's own fallback. The node answers
    /// [`NodeToCore::SettingsApplied`]. A change takes effect for the next dispatcher call.
    /// `banned` (v2): network-wide ban list; the node refuses these ISSIs and kicks them.
    Settings { dispatcher_issi: Option<Issi>, banned: Vec<Issi> },
    // ----- v1 -----
    /// Assign (`attach`) or remove a group on a registered radio (SS-DGNA, EN 300 392-12-22).
    Dgna { req_id: ReqId, issi: Issi, gssi: Gssi, attach: bool, mnemonic: Option<String> },
    /// Send a text SDS (SDS-TL simple text, EN 300 392-2 §29.5.3) from the dispatcher ISSI.
    SendSds { req_id: ReqId, dest_ssi: u32, dest_is_group: bool, text: String },
    /// Send the full current state again: `MsRegistered` + `MsGroups` (+ last `MsPosition`)
    /// for every registered radio, and `SettingsApplied`. Nodes also do this after every Hello.
    ResyncState,
    // ----- v2 -----
    /// Operator action on one radio, answered with [`NodeToCore::CommandResult`].
    RadioCommand { req_id: ReqId, issi: Issi, action: RadioAction },
    // ----- v5 -----
    /// Send a pre-coded status (D-STATUS) from the dispatcher ISSI. Answered with `CommandResult`.
    SendStatus { req_id: ReqId, dest_ssi: u32, dest_is_group: bool, status: u16 },
    /// Acknowledge a radio's emergency alarm: D-STATUS "Emergency" from the address the alarm
    /// was sent to (`from_ssi`) back to the radio, which ends its alarm retries ("console
    /// acknowledge" in the terminal's programming). Answered with `CommandResult`.
    AckEmergency { req_id: ReqId, issi: Issi, from_ssi: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Encode + for<'a> Decode<'a> + PartialEq + std::fmt::Debug>(value: T) {
        let bytes = encode(&value);
        let back: T = decode(&bytes).expect("decode");
        assert_eq!(back, value);
    }

    #[test]
    fn node_messages_roundtrip() {
        roundtrip(NodeToCore::Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: "gdansk-01".into(),
            software: "flowstation 0.4.0".into(),
            token: "secret".into(),
            dispatcher_issi: Some(9_000_001),
            dispatch_gssis: vec![91, 401],
        });
        roundtrip(NodeToCore::SettingsApplied { dispatcher_issi: None, error: Some("ISSI in use".into()), banned: 2 });
        roundtrip(NodeToCore::NodeStatus(NodeStatus {
            uptime_s: 3600,
            carriers: vec![CarrierInfo { number: 1529, dl_hz: 438_225_000, ul_hz: 430_625_000 }],
            mcc: 260,
            mnc: 99,
            location_area: 1,
            registered: 4,
            active_calls: 1,
            timeslots_total: 3,
            cpu_temp_c: Some(51.5),
        }));
        roundtrip(NodeToCore::MsPosition { issi: 7, lat: 54.352, lon: 18.646, accuracy_m: Some(10.0), speed_kmh: None, heading_deg: Some(90.0) });
        roundtrip(NodeToCore::CommandResult { req_id: 4, ok: false, detail: Some("radio not registered".into()) });
        roundtrip(NodeToCore::GroupTxRejected { req_id: 7, reason: RejectReason::BrewRoutable });
        roundtrip(NodeToCore::GroupCallStarted {
            call_id: 218,
            gssi: 401,
            caller_issi: 2_603_877,
            priority: CallPriority::EMERGENCY,
        });
        roundtrip(NodeToCore::TxForcedStop { call_id: 218, reason: StopReason::LinkLost });
    }

    #[test]
    fn core_messages_roundtrip() {
        roundtrip(CoreToNode::GroupTxRequest {
            req_id: 1,
            gssi: 401,
            priority: CallPriority::new(11).unwrap(),
        });
        roundtrip(CoreToNode::MonitorSet { all: false, gssis: vec![20, 91] });
        roundtrip(CoreToNode::Settings { dispatcher_issi: Some(9_000_001), banned: vec![2_601_292] });
        roundtrip(CoreToNode::RadioCommand { req_id: 5, issi: 2_601_292, action: RadioAction::RequestPosition });
        roundtrip(CoreToNode::Dgna { req_id: 3, issi: 2_603_877, gssi: 401, attach: true, mnemonic: Some("FIRE 1".into()) });
        roundtrip(CoreToNode::SendSds { req_id: 4, dest_ssi: 10, dest_is_group: true, text: "Zażółć gęślą jaźń / Привіт".into() });
        roundtrip(CoreToNode::ResyncState);
        roundtrip(CoreToNode::HelloAck { protocol_version: 0, accepted: false, reason: Some("bad token".into()) });
    }

    #[test]
    fn bus_json_is_stable_and_readable() {
        let json = serde_json::to_string(&NodeToCore::MsRegistered { issi: 2_603_877 }).unwrap();
        assert_eq!(json, r#"{"MsRegistered":{"issi":2603877}}"#);
    }
}
