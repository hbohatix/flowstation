//! Dispatcher link worker: runs on its own thread, owns the network transport.
//!
//! - connects to the core, sends `Hello`, waits for `HelloAck`;
//! - relays `CoreToNode` messages and downlink voice to the entity, and the entity's
//!   messages / uplink voice to the core;
//! - turns telemetry (registrations, groups, calls, emergencies) into `NodeToCore` state events;
//! - applies the dispatcher ISSI setting (refusing an ISSI held by a registered radio);
//! - executes console commands (DGNA, SDS, kick, re-register, position request) through the
//!   control channels, like the dashboard;
//! - enforces the platform ban list and reports the station's status every 10 s;
//! - heartbeats both ways; a silent link is torn down and re-established with backoff.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use dispatch_proto::{
    CallPriority, CarrierInfo, CoreToNode, Gssi, Issi, NodeStatus, NodeToCore, PROTOCOL_VERSION, RadioAction, VoiceDatagram,
    settings::validate_issi,
};
use tetra_config::bluestation::SharedConfig;

use crate::net_control::ControlCommand;
use crate::net_telemetry::{TelemetryEvent, TelemetrySource};
use crate::network::transports::NetworkTransport;

/// Heartbeat interval towards the core (the core uses the same).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait for `HelloAck` after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Upper bound on items handled per loop iteration from each internal queue.
const MAX_PER_ITERATION: usize = 256;
/// RSSI of one radio is reported to the core at most this often.
const RSSI_INTERVAL: Duration = Duration::from_secs(5);
/// Station status report interval.
const STATUS_INTERVAL: Duration = Duration::from_secs(10);

/// LIP IMMEDIATE LOCATION REPORT REQUEST (TS 100 392-18-1 §6.2.16) as an SDS type-4 SDU:
/// protocol identifier 10, then PDU type `01` (long), extension `0001` (immediate location
/// report request), request/response `0`, report type `01` (long report, time elapsed) =
/// 9 bits `010001001`.
pub const LIP_IMMEDIATE_REQUEST: [u8; 3] = [0x0A, 0b0100_0100, 0b1000_0000];
pub const LIP_IMMEDIATE_REQUEST_BITS: u16 = 8 + 9;

/// Entity → worker.
#[derive(Debug)]
pub enum DispatcherCommand {
    /// Send a control/state message to the core (dropped while the link is down).
    Send(NodeToCore),
    /// Send uplink voice to the core (dropped while the link is down).
    Voice(VoiceDatagram),
    /// Stop the worker thread.
    Shutdown,
}

/// Worker → entity.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatcherEvent {
    LinkUp,
    LinkDown,
    /// Effective dispatcher ISSI after applying the platform setting (None = no ISSI available).
    DispatcherIssi(Option<Issi>),
    /// A control message from the core for the entity (GroupTxRequest, GroupTxStop, MonitorSet).
    FromCore(CoreToNode),
    /// Downlink voice from the core.
    VoiceDl(VoiceDatagram),
}

/// Static parameters the worker needs (from `[dispatcher]`).
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub node_id: String,
    pub token: String,
    pub software: String,
    pub fallback_issi: Option<Issi>,
    pub dispatch_gssis: Vec<u32>,
    pub link_timeout: Duration,
    /// SS-DGNA attachment mode for console DGNA (`cell_info.dgna_attachment_mode`, table 16.51).
    pub dgna_attachment_mode: u8,
}

/// Outcome of applying a requested dispatcher ISSI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedIssi {
    pub effective: Option<Issi>,
    pub error: Option<String>,
}

/// Pure settings logic (unit-tested): requested platform value, else the node's fallback;
/// never an invalid SSI and never an ISSI that a registered radio uses.
pub fn apply_dispatcher_issi(requested: Option<Issi>, fallback: Option<Issi>, registered: &HashSet<Issi>) -> AppliedIssi {
    let usable_fallback = fallback.filter(|f| validate_issi(*f).is_ok() && !registered.contains(f));
    match requested {
        None => match fallback {
            Some(f) if registered.contains(&f) => AppliedIssi {
                effective: None,
                error: Some(format!("fallback ISSI {f} is used by a registered radio")),
            },
            _ => AppliedIssi { effective: usable_fallback, error: None },
        },
        Some(issi) => {
            if let Err(e) = validate_issi(issi) {
                AppliedIssi { effective: usable_fallback, error: Some(e.to_string()) }
            } else if registered.contains(&issi) {
                AppliedIssi {
                    effective: usable_fallback,
                    error: Some(format!("ISSI {issi} is used by a registered radio")),
                }
            } else {
                AppliedIssi { effective: Some(issi), error: None }
            }
        }
    }
}

/// Map a telemetry event to the state event the core wants, if any.
pub fn telemetry_to_core(event: &TelemetryEvent) -> Option<NodeToCore> {
    Some(match event {
        TelemetryEvent::MsRegistration { issi } => NodeToCore::MsRegistered { issi: *issi },
        TelemetryEvent::MsDeregistration { issi } => NodeToCore::MsDeregistered { issi: *issi },
        TelemetryEvent::MsGroupsSnapshot { issi, gssis } => NodeToCore::MsGroups { issi: *issi, gssis: gssis.clone() },
        TelemetryEvent::GroupCallStarted { call_id, gssi, caller_issi, priority, .. } => NodeToCore::GroupCallStarted {
            call_id: *call_id,
            gssi: *gssi,
            caller_issi: *caller_issi,
            priority: CallPriority::new(*priority).unwrap_or(CallPriority::NOT_DEFINED),
        },
        TelemetryEvent::GroupCallEnded { call_id, gssi } => NodeToCore::GroupCallEnded { call_id: *call_id, gssi: *gssi },
        TelemetryEvent::CallSpeakerChanged { call_id, is_group: true, dest_addr, speaker_issi, .. } => {
            NodeToCore::SpeakerChanged { call_id: *call_id, gssi: *dest_addr, speaker_issi: *speaker_issi }
        }
        TelemetryEvent::EmergencyAlarm { source_issi, dest_ssi } => NodeToCore::Emergency { issi: *source_issi, gssi: *dest_ssi },
        TelemetryEvent::EmergencyCancel { source_issi } => NodeToCore::EmergencyCleared { issi: *source_issi },
        TelemetryEvent::MsPosition { issi, position } => NodeToCore::MsPosition {
            issi: *issi,
            lat: position.lat,
            lon: position.lon,
            accuracy_m: position.position_error_m.or(position.horizontal_uncertainty_m),
            speed_kmh: position.speed_kmh,
            heading_deg: position.heading_deg,
        },
        TelemetryEvent::DgnaStatus(info) => NodeToCore::DgnaStatus {
            issi: info.issi,
            gssi: info.gssi,
            attach: info.attach,
            accepted: info.accepted,
            detail: info.detail.clone(),
        },
        TelemetryEvent::SdsLog { source_issi, dest_issi, is_group, text, .. } if !text.is_empty() => NodeToCore::SdsReceived {
            source_issi: *source_issi,
            dest_ssi: *dest_issi,
            is_group: *is_group,
            text: text.clone(),
        },
        _ => return None,
    })
}

pub struct DispatcherWorker<T: NetworkTransport> {
    cfg: WorkerConfig,
    transport: T,
    events: Sender<DispatcherEvent>,
    commands: Receiver<DispatcherCommand>,
    telemetry: Option<TelemetrySource>,
    /// CMCE control channel (DGNA, SDS, kick), the same one the dashboard uses.
    cmce: Option<Sender<ControlCommand>>,
    /// MM control channel (re-registration).
    mm: Option<Sender<ControlCommand>>,
    /// Station config/state: ban list, status report. `None` in unit tests.
    station: Option<SharedConfig>,
    /// Network-wide ban list from the platform (sorted).
    banned: Vec<Issi>,
    /// Group calls in progress (for the status report).
    active_calls: HashSet<u16>,
    started_at: Instant,
    connected: bool,
    registered: HashSet<Issi>,
    /// Last known groups and position per registered radio, replayed in state snapshots.
    groups: HashMap<Issi, Vec<Gssi>>,
    positions: HashMap<Issi, NodeToCore>,
    rssi_sent: HashMap<Issi, Instant>,
    /// Last dispatcher ISSI requested by the core (None = use fallback).
    requested_issi: Option<Issi>,
    effective_issi: Option<Issi>,
    hb_seq: u32,
}

impl<T: NetworkTransport> DispatcherWorker<T> {
    pub fn new(
        cfg: WorkerConfig,
        transport: T,
        events: Sender<DispatcherEvent>,
        commands: Receiver<DispatcherCommand>,
        telemetry: Option<TelemetrySource>,
        cmce: Option<Sender<ControlCommand>>,
    ) -> Self {
        let effective_issi = cfg.fallback_issi;
        Self {
            cfg,
            transport,
            events,
            commands,
            telemetry,
            cmce,
            connected: false,
            registered: HashSet::new(),
            groups: HashMap::new(),
            positions: HashMap::new(),
            mm: None,
            station: None,
            banned: Vec::new(),
            active_calls: HashSet::new(),
            started_at: Instant::now(),
            rssi_sent: HashMap::new(),
            requested_issi: None,
            effective_issi,
            hb_seq: 0,
        }
    }

    /// Give the worker the station's config/state (ban list, status report).
    pub fn with_station(mut self, station: SharedConfig) -> Self {
        self.station = Some(station);
        self
    }

    /// The MM control channel (operator re-registration requests).
    pub fn with_mm(mut self, mm: Option<Sender<ControlCommand>>) -> Self {
        self.mm = mm;
        self
    }

    fn emit(&self, event: DispatcherEvent) {
        match self.events.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(ev)) => tracing::warn!("Dispatcher worker: entity queue full, dropping {:?}", ev),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn send(&mut self, msg: &NodeToCore) -> bool {
        match self.transport.send_reliable(&dispatch_proto::encode(msg)) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("Dispatcher: send to core failed: {:?}", e);
                false
            }
        }
    }

    /// Blocking main loop. Returns on `Shutdown` or when the entity is gone.
    pub fn run(&mut self) {
        let mut backoff = BACKOFF_MIN;
        loop {
            if !self.connected {
                match self.connect_and_hello() {
                    Ok(()) => {
                        backoff = BACKOFF_MIN;
                        self.connected = true;
                        self.emit(DispatcherEvent::LinkUp);
                        tracing::info!("Dispatcher: link to core is up");
                    }
                    Err(reason) => {
                        tracing::warn!("Dispatcher: cannot reach core ({}); retrying in {:?}", reason, backoff);
                        if !self.idle_for(backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                }
            }
            match self.session() {
                SessionEnd::Shutdown => {
                    self.transport.disconnect();
                    return;
                }
                SessionEnd::Lost(reason) => {
                    tracing::warn!("Dispatcher: link to core lost: {}", reason);
                    self.transport.disconnect();
                    self.connected = false;
                    self.emit(DispatcherEvent::LinkDown);
                }
            }
        }
    }

    /// While disconnected: drop entity traffic (it cannot be delivered) but keep tracking
    /// registrations, and honour `Shutdown`. Returns false on shutdown.
    fn idle_for(&mut self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if !self.drain_commands_while_down() {
                return false;
            }
            self.track_telemetry_only();
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn drain_commands_while_down(&mut self) -> bool {
        for _ in 0..MAX_PER_ITERATION {
            match self.commands.try_recv() {
                Ok(DispatcherCommand::Shutdown) => return false,
                Ok(_) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return false,
            }
        }
        true
    }

    fn track_telemetry_only(&mut self) {
        let Some(source) = &self.telemetry else { return };
        let mut seen = Vec::new();
        for _ in 0..MAX_PER_ITERATION {
            match source.try_recv() {
                Some(ev) => seen.push(ev),
                None => break,
            }
        }
        for ev in &seen {
            self.track_registration(ev);
        }
    }

    fn track_registration(&mut self, event: &TelemetryEvent) {
        match event {
            TelemetryEvent::MsRegistration { issi } => {
                self.registered.insert(*issi);
            }
            TelemetryEvent::MsDeregistration { issi } | TelemetryEvent::MsTimeoutDrop { issi } => {
                self.registered.remove(issi);
                self.groups.remove(issi);
                self.positions.remove(issi);
                self.rssi_sent.remove(issi);
            }
            TelemetryEvent::MsGroupsSnapshot { issi, gssis } => {
                self.groups.insert(*issi, gssis.clone());
            }
            TelemetryEvent::GroupCallStarted { call_id, .. } => {
                self.active_calls.insert(*call_id);
            }
            TelemetryEvent::GroupCallEnded { call_id, .. } => {
                self.active_calls.remove(call_id);
            }
            TelemetryEvent::MsPosition { issi, .. } => {
                if let Some(msg) = telemetry_to_core(event) {
                    self.positions.insert(*issi, msg);
                }
            }
            _ => {}
        }
    }

    fn connect_and_hello(&mut self) -> Result<(), String> {
        self.transport.connect().map_err(|e| format!("connect: {e:?}"))?;
        let hello = NodeToCore::Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: self.cfg.node_id.clone(),
            software: self.cfg.software.clone(),
            token: self.cfg.token.clone(),
            dispatcher_issi: self.cfg.fallback_issi,
            dispatch_gssis: self.cfg.dispatch_gssis.clone(),
        };
        if !self.send(&hello) {
            return Err("cannot send Hello".into());
        }
        let deadline = Instant::now() + HELLO_TIMEOUT;
        while Instant::now() < deadline {
            for msg in self.transport.receive_reliable() {
                match dispatch_proto::decode::<CoreToNode>(&msg.payload) {
                    Ok(CoreToNode::HelloAck { accepted: true, .. }) => return Ok(()),
                    Ok(CoreToNode::HelloAck { accepted: false, reason, .. }) => {
                        return Err(format!("core refused this node: {}", reason.unwrap_or_default()));
                    }
                    Ok(other) => tracing::debug!("Dispatcher: ignoring {:?} before HelloAck", other),
                    Err(e) => tracing::warn!("Dispatcher: undecodable message before HelloAck: {}", e),
                }
            }
            if !self.drain_commands_while_down() {
                return Err("shutdown".into());
            }
        }
        Err("no HelloAck in time".into())
    }

    /// One connected session. Returns why it ended.
    /// The full subscriber state, so a core or console that starts late sees every radio.
    fn state_snapshot(&self) -> Vec<NodeToCore> {
        let mut issis: Vec<Issi> = self.registered.iter().copied().collect();
        issis.sort_unstable();
        let mut out = Vec::new();
        for issi in issis {
            out.push(NodeToCore::MsRegistered { issi });
            if let Some(gssis) = self.groups.get(&issi) {
                out.push(NodeToCore::MsGroups { issi, gssis: gssis.clone() });
            }
            if let Some(pos) = self.positions.get(&issi) {
                out.push(pos.clone());
            }
        }
        out
    }

    fn send_snapshot(&mut self) -> bool {
        let snapshot = self.state_snapshot();
        tracing::info!("Dispatcher: sending state of {} registered radio(s)", self.registered.len());
        snapshot.iter().all(|m| self.send(m))
    }

    fn session(&mut self) -> SessionEnd {
        // Radios that registered before this session (or while the link was down).
        if !self.send_snapshot() || !self.send_status() {
            return SessionEnd::Lost("send failed".into());
        }
        let mut last_status = Instant::now();
        let mut last_rx = Instant::now();
        let mut last_hb = Instant::now().checked_sub(HEARTBEAT_INTERVAL).unwrap_or_else(Instant::now);
        loop {
            // From the core: control (one frame per call, bounded wait inside the transport).
            for msg in self.transport.receive_reliable() {
                last_rx = Instant::now();
                match dispatch_proto::decode::<CoreToNode>(&msg.payload) {
                    Ok(CoreToNode::Heartbeat { .. }) | Ok(CoreToNode::HelloAck { .. }) => {}
                    Ok(CoreToNode::Settings { dispatcher_issi, banned }) => {
                        self.requested_issi = dispatcher_issi;
                        self.apply_bans(banned);
                        if !self.reapply_settings(true) {
                            return SessionEnd::Lost("send failed".into());
                        }
                    }
                    Ok(CoreToNode::ResyncState) => {
                        if !self.send_snapshot() || !self.reapply_settings(true) {
                            return SessionEnd::Lost("send failed".into());
                        }
                    }
                    Ok(cmd @ (CoreToNode::Dgna { .. } | CoreToNode::SendSds { .. } | CoreToNode::RadioCommand { .. })) => {
                        let result = self.run_console_command(cmd);
                        if !self.send(&result) {
                            return SessionEnd::Lost("send failed".into());
                        }
                    }
                    Ok(other) => self.emit(DispatcherEvent::FromCore(other)),
                    Err(e) => tracing::warn!("Dispatcher: undecodable control message: {}", e),
                }
            }
            // From the core: voice.
            for msg in self.transport.receive_unreliable() {
                last_rx = Instant::now();
                match dispatch_proto::decode::<VoiceDatagram>(&msg.payload) {
                    Ok(frame @ VoiceDatagram::Dl { .. }) => self.emit(DispatcherEvent::VoiceDl(frame)),
                    Ok(VoiceDatagram::Ul { .. }) => tracing::debug!("Dispatcher: ignoring uplink frame from core"),
                    Err(e) => tracing::debug!("Dispatcher: undecodable voice datagram: {}", e),
                }
            }
            // From the entity.
            for _ in 0..MAX_PER_ITERATION {
                match self.commands.try_recv() {
                    Ok(DispatcherCommand::Send(msg)) => {
                        if !self.send(&msg) {
                            return SessionEnd::Lost("send failed".into());
                        }
                    }
                    Ok(DispatcherCommand::Voice(frame)) => {
                        // Voice is best effort: a lost datagram must not tear the link down.
                        if let Err(e) = self.transport.send_unreliable(&dispatch_proto::encode(&frame)) {
                            tracing::debug!("Dispatcher: voice datagram not sent: {:?}", e);
                        }
                    }
                    Ok(DispatcherCommand::Shutdown) => return SessionEnd::Shutdown,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return SessionEnd::Shutdown,
                }
            }
            // From telemetry: subscriber and call state.
            if let Some(end) = self.forward_telemetry() {
                return end;
            }
            if last_status.elapsed() >= STATUS_INTERVAL {
                if !self.send_status() {
                    return SessionEnd::Lost("status send failed".into());
                }
                last_status = Instant::now();
            }
            // Heartbeat and liveness.
            if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                self.hb_seq = self.hb_seq.wrapping_add(1);
                if !self.send(&NodeToCore::Heartbeat { seq: self.hb_seq }) {
                    return SessionEnd::Lost("heartbeat send failed".into());
                }
                last_hb = Instant::now();
            }
            if last_rx.elapsed() > self.cfg.link_timeout {
                return SessionEnd::Lost(format!("no traffic from core for {:?}", self.cfg.link_timeout));
            }
        }
    }

    fn forward_telemetry(&mut self) -> Option<SessionEnd> {
        let source = self.telemetry.as_ref()?;
        let mut batch = Vec::new();
        for _ in 0..MAX_PER_ITERATION {
            match source.try_recv() {
                Some(ev) => batch.push(ev),
                None => break,
            }
        }
        for ev in batch {
            self.track_registration(&ev);
            if let TelemetryEvent::MsRssi { issi, rssi_dbfs } = ev {
                let due = self.rssi_sent.get(&issi).is_none_or(|t| t.elapsed() >= RSSI_INTERVAL);
                if due {
                    self.rssi_sent.insert(issi, Instant::now());
                    if !self.send(&NodeToCore::MsRssi { issi, rssi_dbfs }) {
                        return Some(SessionEnd::Lost("send failed".into()));
                    }
                }
                continue;
            }
            // A radio registering with the dispatcher's ISSI makes that ISSI unusable.
            if matches!(ev, TelemetryEvent::MsRegistration { issi } if Some(issi) == self.effective_issi)
                && !self.reapply_settings(true)
            {
                return Some(SessionEnd::Lost("send failed".into()));
            }
            if let Some(msg) = telemetry_to_core(&ev)
                && !self.send(&msg)
            {
                return Some(SessionEnd::Lost("send failed".into()));
            }
        }
        None
    }

    /// DGNA / SDS from the console, executed through the CMCE control channel.
    fn run_console_command(&mut self, cmd: CoreToNode) -> NodeToCore {
        let (req_id, control) = match cmd {
            CoreToNode::Dgna { req_id, issi, gssi, attach, mnemonic } => (
                req_id,
                Ok(ControlCommand::Dgna { issi, gssi, mnemonic, attachment_mode: self.cfg.dgna_attachment_mode, attach }),
            ),
            CoreToNode::SendSds { req_id, dest_ssi, dest_is_group, text } => {
                let built = match (self.effective_issi, dispatch_proto::sds::encode_text(&text)) {
                    (None, _) => Err("no dispatcher ISSI to send from".to_string()),
                    (_, Err(e)) => Err(e.to_string()),
                    (Some(source_ssi), Ok(payload)) => Ok(ControlCommand::SendSds {
                        handle: req_id,
                        source_ssi,
                        dest_ssi,
                        dest_is_group,
                        len_bits: (payload.len() * 8) as u16,
                        payload,
                    }),
                };
                (req_id, built)
            }
            CoreToNode::RadioCommand { req_id, issi, action } => {
                tracing::info!("Dispatcher: console {:?} on ISSI {}", action, issi);
                let built = match action {
                    RadioAction::Kick => Ok(ControlCommand::KickMs { issi }),
                    RadioAction::ReRegister => Ok(ControlCommand::ReRegister { issi }),
                    RadioAction::RequestPosition => match self.effective_issi {
                        None => Err("no dispatcher ISSI to send from".to_string()),
                        Some(source_ssi) => Ok(ControlCommand::SendRawSdsType4 {
                            handle: req_id,
                            source_ssi,
                            dest_ssi: issi,
                            dest_is_group: false,
                            len_bits: LIP_IMMEDIATE_REQUEST_BITS,
                            payload: LIP_IMMEDIATE_REQUEST.to_vec(),
                        }),
                    },
                };
                (req_id, built)
            }
            _ => return NodeToCore::CommandResult { req_id: 0, ok: false, detail: Some("not a console command".into()) },
        };
        let outcome = control.and_then(|control| {
            // Registration is Mobility Management; everything else goes through CMCE.
            let (channel, name) = match control {
                ControlCommand::ReRegister { .. } => (&self.mm, "MM"),
                _ => (&self.cmce, "CMCE"),
            };
            match channel {
                Some(tx) => tx.send(control).map_err(|_| format!("{name} control channel closed")),
                None => Err(format!("{name} control channel not available")),
            }
        });
        match outcome {
            Ok(()) => NodeToCore::CommandResult { req_id, ok: true, detail: None },
            Err(detail) => {
                tracing::warn!("Dispatcher: console command {} refused: {}", req_id, detail);
                NodeToCore::CommandResult { req_id, ok: false, detail: Some(detail) }
            }
        }
    }

    /// Take over the platform's ban list: store it for MM's access check and kick every banned
    /// radio that is registered right now.
    fn apply_bans(&mut self, mut banned: Vec<Issi>) {
        banned.sort_unstable();
        banned.dedup();
        if banned == self.banned {
            return;
        }
        if let Some(station) = &self.station {
            station.state_write().issi_banlist = banned.clone();
        }
        let to_kick: Vec<Issi> = self.registered.iter().copied().filter(|i| banned.binary_search(i).is_ok()).collect();
        tracing::info!("Dispatcher: {} ISSI(s) banned by the platform, kicking {} registered", banned.len(), to_kick.len());
        for issi in to_kick {
            if let Some(tx) = &self.cmce {
                let _ = tx.send(ControlCommand::KickMs { issi });
            }
        }
        self.banned = banned;
    }

    fn node_status(&self) -> NodeStatus {
        let mut status = NodeStatus {
            uptime_s: self.started_at.elapsed().as_secs(),
            carriers: Vec::new(),
            mcc: 0,
            mnc: 0,
            location_area: 0,
            registered: self.registered.len() as u32,
            active_calls: self.active_calls.len() as u32,
            timeslots_total: 0,
            cpu_temp_c: cpu_temperature(),
        };
        if let Some(station) = &self.station {
            let cfg = station.config();
            status.mcc = cfg.net.mcc;
            status.mnc = cfg.net.mnc;
            status.location_area = cfg.cell.location_area;
            if let Ok(carriers) = cfg.bs_phase_mod_carriers() {
                // Four timeslots per carrier; timeslot 1 of the main carrier is the MCCH.
                status.timeslots_total = (carriers.len() as u32 * 4).saturating_sub(1);
                status.carriers = carriers.into_iter().map(|(number, dl, ul)| CarrierInfo { number, dl_hz: dl.into(), ul_hz: ul.into() }).collect();
            }
        }
        status
    }

    fn send_status(&mut self) -> bool {
        let status = self.node_status();
        self.send(&NodeToCore::NodeStatus(status))
    }

    /// Recompute the effective ISSI, tell the entity, optionally report to the core.
    fn reapply_settings(&mut self, report: bool) -> bool {
        let applied = apply_dispatcher_issi(self.requested_issi, self.cfg.fallback_issi, &self.registered);
        if applied.effective != self.effective_issi {
            tracing::info!("Dispatcher: ISSI now {:?}", applied.effective);
        }
        if let Some(err) = &applied.error {
            tracing::warn!("Dispatcher: settings not fully applied: {}", err);
        }
        self.effective_issi = applied.effective;
        self.emit(DispatcherEvent::DispatcherIssi(applied.effective));
        if report {
            return self.send(&NodeToCore::SettingsApplied {
                dispatcher_issi: applied.effective,
                error: applied.error,
                banned: self.banned.len() as u32,
            });
        }
        true
    }
}

/// SoC temperature in °C (Raspberry Pi and most Linux boards), if readable.
fn cpu_temperature() -> Option<f32> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    raw.trim().parse::<f32>().ok().map(|milli| milli / 1000.0)
}

enum SessionEnd {
    Shutdown,
    Lost(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[u32]) -> HashSet<u32> {
        v.iter().copied().collect()
    }

    #[test]
    fn requested_issi_wins_when_free() {
        let a = apply_dispatcher_issi(Some(9_100_000), Some(9_000_001), &set(&[]));
        assert_eq!(a, AppliedIssi { effective: Some(9_100_000), error: None });
    }

    #[test]
    fn empty_setting_uses_fallback() {
        let a = apply_dispatcher_issi(None, Some(9_000_001), &set(&[]));
        assert_eq!(a, AppliedIssi { effective: Some(9_000_001), error: None });
        assert_eq!(apply_dispatcher_issi(None, None, &set(&[])).effective, None);
    }

    #[test]
    fn issi_of_registered_radio_is_refused() {
        let a = apply_dispatcher_issi(Some(2_603_877), Some(9_000_001), &set(&[2_603_877]));
        assert_eq!(a.effective, Some(9_000_001));
        assert!(a.error.unwrap().contains("2603877"));
        // Fallback itself taken by a radio: no usable ISSI at all.
        let b = apply_dispatcher_issi(None, Some(2_603_877), &set(&[2_603_877]));
        assert_eq!(b.effective, None);
        assert!(b.error.is_some());
    }

    #[test]
    fn broadcast_issi_is_refused() {
        let a = apply_dispatcher_issi(Some(0xFF_FFFF), Some(9_000_001), &set(&[]));
        assert_eq!(a.effective, Some(9_000_001));
        assert!(a.error.is_some());
    }

    // ----- worker against a fake transport -----

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use crate::network::transports::{NetworkAddress, NetworkError, NetworkMessage};

    #[derive(Default)]
    struct FakeState {
        inbound: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    #[derive(Clone, Default)]
    struct FakeTransport(Arc<Mutex<FakeState>>);

    impl FakeTransport {
        fn push(&self, msg: &CoreToNode) {
            self.0.lock().unwrap().inbound.push_back(dispatch_proto::encode(msg));
        }
        fn sent(&self) -> Vec<NodeToCore> {
            self.0.lock().unwrap().sent.iter().filter_map(|b| dispatch_proto::decode(b).ok()).collect()
        }
    }

    fn msg(payload: Vec<u8>) -> NetworkMessage {
        NetworkMessage { source: NetworkAddress::Udp { host: "core".into(), port: 4433 }, payload, timestamp: Instant::now() }
    }

    impl NetworkTransport for FakeTransport {
        fn connect(&mut self) -> Result<(), NetworkError> {
            Ok(())
        }
        fn send_reliable(&mut self, payload: &[u8]) -> Result<(), NetworkError> {
            self.0.lock().unwrap().sent.push(payload.to_vec());
            Ok(())
        }
        fn send_unreliable(&mut self, _payload: &[u8]) -> Result<(), NetworkError> {
            Ok(())
        }
        fn receive_reliable(&mut self) -> Vec<NetworkMessage> {
            std::thread::sleep(Duration::from_millis(1));
            self.0.lock().unwrap().inbound.pop_front().map(msg).into_iter().collect()
        }
        fn receive_unreliable(&mut self) -> Vec<NetworkMessage> {
            Vec::new()
        }
        fn wait_for_response_reliable(&mut self) -> Result<NetworkMessage, NetworkError> {
            Err(NetworkError::Timeout)
        }
    }

    fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for: {what}");
    }

    #[test]
    fn worker_session_hello_settings_telemetry_shutdown() {
        let transport = FakeTransport::default();
        transport.push(&CoreToNode::HelloAck { protocol_version: PROTOCOL_VERSION, accepted: true, reason: None });
        let (event_tx, event_rx) = crossbeam_channel::bounded(64);
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
        let (tsink, tsource) = crate::net_telemetry::telemetry_channel();
        let cfg = WorkerConfig {
            node_id: "gdansk-01".into(),
            token: "secret".into(),
            software: "test".into(),
            fallback_issi: Some(9_000_001),
            dispatch_gssis: vec![91],
            link_timeout: Duration::from_secs(30),
            dgna_attachment_mode: 0,
        };
        let (cmce_tx, cmce_rx) = crossbeam_channel::unbounded();
        let fake = transport.clone();
        let handle = std::thread::spawn(move || DispatcherWorker::new(cfg, fake, event_tx, cmd_rx, Some(tsource), Some(cmce_tx)).run());

        // Hello first, then the link is up.
        wait_until("Hello", || matches!(transport.sent().first(), Some(NodeToCore::Hello { node_id, dispatch_gssis, .. }) if node_id == "gdansk-01" && *dispatch_gssis == vec![91]));
        assert_eq!(event_rx.recv_timeout(Duration::from_secs(3)).unwrap(), DispatcherEvent::LinkUp);

        // A radio registers, then the console asks for that radio's ISSI: refused, fallback kept.
        tsink.send(TelemetryEvent::MsRegistration { issi: 2_603_877 });
        wait_until("MsRegistered", || transport.sent().contains(&NodeToCore::MsRegistered { issi: 2_603_877 }));
        transport.push(&CoreToNode::Settings { dispatcher_issi: Some(2_603_877), banned: vec![] });
        wait_until("SettingsApplied", || {
            transport.sent().iter().any(|m| matches!(m, NodeToCore::SettingsApplied { dispatcher_issi: Some(9_000_001), error: Some(_), .. }))
        });
        assert_eq!(event_rx.recv_timeout(Duration::from_secs(3)).unwrap(), DispatcherEvent::DispatcherIssi(Some(9_000_001)));

        // Commands for the entity are relayed.
        transport.push(&CoreToNode::MonitorSet { all: false, gssis: vec![91] });
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            DispatcherEvent::FromCore(CoreToNode::MonitorSet { all: false, gssis: vec![91] })
        );

        // Console commands go to the CMCE control channel and are acknowledged to the core.
        transport.push(&CoreToNode::Dgna { req_id: 21, issi: 2_603_877, gssi: 401, attach: true, mnemonic: None });
        match cmce_rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            ControlCommand::Dgna { issi: 2_603_877, gssi: 401, attach: true, .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        transport.push(&CoreToNode::SendSds { req_id: 22, dest_ssi: 10, dest_is_group: true, text: "\u{141}\u{f3}d\u{17a}".into() });
        match cmce_rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            ControlCommand::SendSds { source_ssi: 9_000_001, dest_ssi: 10, dest_is_group: true, payload, len_bits, .. } => {
                assert_eq!(payload[0], 0x02, "UTF-16BE for non-Latin-1 text");
                assert_eq!(usize::from(len_bits), payload.len() * 8);
            }
            other => panic!("unexpected {other:?}"),
        }
        wait_until("command results", || {
            let sent = transport.sent();
            sent.contains(&NodeToCore::CommandResult { req_id: 21, ok: true, detail: None })
                && sent.contains(&NodeToCore::CommandResult { req_id: 22, ok: true, detail: None })
        });

        // Operator radio commands: kick → CMCE, position request → raw LIP SDS from the
        // dispatcher ISSI; re-register needs MM, which this test does not wire → refused.
        transport.push(&CoreToNode::RadioCommand { req_id: 31, issi: 2_603_877, action: RadioAction::Kick });
        assert!(matches!(cmce_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ControlCommand::KickMs { issi: 2_603_877 }));
        transport.push(&CoreToNode::RadioCommand { req_id: 32, issi: 2_603_877, action: RadioAction::RequestPosition });
        match cmce_rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            ControlCommand::SendRawSdsType4 { dest_ssi: 2_603_877, dest_is_group: false, len_bits: 17, payload, source_ssi: 9_000_001, .. } => {
                assert_eq!(payload, vec![0x0A, 0x44, 0x80], "LIP immediate location report request");
            }
            other => panic!("unexpected {other:?}"),
        }
        transport.push(&CoreToNode::RadioCommand { req_id: 33, issi: 2_603_877, action: RadioAction::ReRegister });
        wait_until("radio command results", || {
            let sent = transport.sent();
            sent.contains(&NodeToCore::CommandResult { req_id: 31, ok: true, detail: None })
                && sent.contains(&NodeToCore::CommandResult { req_id: 32, ok: true, detail: None })
                && sent.iter().any(|m| matches!(m, NodeToCore::CommandResult { req_id: 33, ok: false, .. }))
        });

        // A ban on a registered radio kicks it and is reported back.
        transport.push(&CoreToNode::Settings { dispatcher_issi: None, banned: vec![2_603_877] });
        assert!(matches!(cmce_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ControlCommand::KickMs { issi: 2_603_877 }));
        wait_until("SettingsApplied with ban", || {
            transport.sent().iter().any(|m| matches!(m, NodeToCore::SettingsApplied { banned: 1, .. }))
        });

        // Status report right after Hello.
        assert!(transport.sent().iter().any(|m| matches!(m, NodeToCore::NodeStatus(s) if s.registered == 0)), "status sent after Hello");

        // A resync replays the radio with its groups, then the settings.
        tsink.send(TelemetryEvent::MsGroupsSnapshot { issi: 2_603_877, gssis: vec![91, 401] });
        wait_until("MsGroups", || transport.sent().contains(&NodeToCore::MsGroups { issi: 2_603_877, gssis: vec![91, 401] }));
        let before = transport.sent().len();
        transport.push(&CoreToNode::ResyncState);
        wait_until("resync", || {
            let after = transport.sent()[before..].to_vec();
            after.contains(&NodeToCore::MsRegistered { issi: 2_603_877 })
                && after.contains(&NodeToCore::MsGroups { issi: 2_603_877, gssis: vec![91, 401] })
                && after.iter().any(|m| matches!(m, NodeToCore::SettingsApplied { .. }))
        });

        cmd_tx.send(DispatcherCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn refused_hello_reconnects_later() {
        let transport = FakeTransport::default();
        transport.push(&CoreToNode::HelloAck { protocol_version: PROTOCOL_VERSION, accepted: false, reason: Some("bad token".into()) });
        let (event_tx, event_rx) = crossbeam_channel::bounded(64);
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
        let cfg = WorkerConfig {
            node_id: "n".into(),
            token: "wrong".into(),
            software: "test".into(),
            fallback_issi: None,
            dispatch_gssis: vec![91],
            link_timeout: Duration::from_secs(30),
            dgna_attachment_mode: 0,
        };
        let fake = transport.clone();
        let handle = std::thread::spawn(move || DispatcherWorker::new(cfg, fake, event_tx, cmd_rx, None, None).run());
        std::thread::sleep(Duration::from_millis(300));
        assert!(event_rx.try_recv().is_err(), "a refused node must not report LinkUp");
        cmd_tx.send(DispatcherCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn telemetry_mapping() {
        assert_eq!(
            telemetry_to_core(&TelemetryEvent::MsRegistration { issi: 7 }),
            Some(NodeToCore::MsRegistered { issi: 7 })
        );
        assert_eq!(
            telemetry_to_core(&TelemetryEvent::CallSpeakerChanged {
                call_id: 3,
                is_group: true,
                dest_addr: 91,
                speaker_issi: 7,
                carrier_num: 0,
                ts: 2
            }),
            Some(NodeToCore::SpeakerChanged { call_id: 3, gssi: 91, speaker_issi: 7 })
        );
        // Individual-call speaker changes are not group state.
        assert_eq!(
            telemetry_to_core(&TelemetryEvent::CallSpeakerChanged {
                call_id: 3,
                is_group: false,
                dest_addr: 8,
                speaker_issi: 7,
                carrier_num: 0,
                ts: 2
            }),
            None
        );
        assert_eq!(telemetry_to_core(&TelemetryEvent::MsRssi { issi: 7, rssi_dbfs: -40.0 }), None);
    }
}
