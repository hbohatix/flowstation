//! Dispatcher link worker: runs on its own thread, owns the network transport.
//!
//! - connects to the core, sends `Hello`, waits for `HelloAck`;
//! - relays `CoreToNode` messages and downlink voice to the entity, and the entity's
//!   messages / uplink voice to the core;
//! - turns telemetry (registrations, groups, calls, emergencies) into `NodeToCore` state events;
//! - applies the dispatcher ISSI setting (refusing an ISSI held by a registered radio);
//! - heartbeats both ways; a silent link is torn down and re-established with backoff.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use dispatch_proto::{CallPriority, CoreToNode, Issi, NodeToCore, PROTOCOL_VERSION, VoiceDatagram, settings::validate_issi};

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
        _ => return None,
    })
}

pub struct DispatcherWorker<T: NetworkTransport> {
    cfg: WorkerConfig,
    transport: T,
    events: Sender<DispatcherEvent>,
    commands: Receiver<DispatcherCommand>,
    telemetry: Option<TelemetrySource>,
    connected: bool,
    registered: HashSet<Issi>,
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
    ) -> Self {
        let effective_issi = cfg.fallback_issi;
        Self {
            cfg,
            transport,
            events,
            commands,
            telemetry,
            connected: false,
            registered: HashSet::new(),
            requested_issi: None,
            effective_issi,
            hb_seq: 0,
        }
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
    fn session(&mut self) -> SessionEnd {
        let mut last_rx = Instant::now();
        let mut last_hb = Instant::now().checked_sub(HEARTBEAT_INTERVAL).unwrap_or_else(Instant::now);
        loop {
            // From the core: control (one frame per call, bounded wait inside the transport).
            for msg in self.transport.receive_reliable() {
                last_rx = Instant::now();
                match dispatch_proto::decode::<CoreToNode>(&msg.payload) {
                    Ok(CoreToNode::Heartbeat { .. }) | Ok(CoreToNode::HelloAck { .. }) => {}
                    Ok(CoreToNode::Settings { dispatcher_issi }) => {
                        self.requested_issi = dispatcher_issi;
                        if !self.reapply_settings(true) {
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
            return self.send(&NodeToCore::SettingsApplied { dispatcher_issi: applied.effective, error: applied.error });
        }
        true
    }
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
        };
        let fake = transport.clone();
        let handle = std::thread::spawn(move || DispatcherWorker::new(cfg, fake, event_tx, cmd_rx, Some(tsource)).run());

        // Hello first, then the link is up.
        wait_until("Hello", || matches!(transport.sent().first(), Some(NodeToCore::Hello { node_id, dispatch_gssis, .. }) if node_id == "gdansk-01" && *dispatch_gssis == vec![91]));
        assert_eq!(event_rx.recv_timeout(Duration::from_secs(3)).unwrap(), DispatcherEvent::LinkUp);

        // A radio registers, then the console asks for that radio's ISSI: refused, fallback kept.
        tsink.send(TelemetryEvent::MsRegistration { issi: 2_603_877 });
        wait_until("MsRegistered", || transport.sent().contains(&NodeToCore::MsRegistered { issi: 2_603_877 }));
        transport.push(&CoreToNode::Settings { dispatcher_issi: Some(2_603_877) });
        wait_until("SettingsApplied", || {
            transport.sent().iter().any(|m| matches!(m, NodeToCore::SettingsApplied { dispatcher_issi: Some(9_000_001), error: Some(_) }))
        });
        assert_eq!(event_rx.recv_timeout(Duration::from_secs(3)).unwrap(), DispatcherEvent::DispatcherIssi(Some(9_000_001)));

        // Commands for the entity are relayed.
        transport.push(&CoreToNode::MonitorSet { gssis: vec![91] });
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            DispatcherEvent::FromCore(CoreToNode::MonitorSet { gssis: vec![91] })
        );

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
        };
        let fake = transport.clone();
        let handle = std::thread::spawn(move || DispatcherWorker::new(cfg, fake, event_tx, cmd_rx, None).run());
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
