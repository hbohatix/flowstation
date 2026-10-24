//! Dispatcher link entity: lives in the real-time loop, no I/O.
//!
//! - `GroupTxRequest` from the core → `NetworkCallStart` to CMCE (owner = Dispatcher);
//!   CMCE's `NetworkCallReady` / `NetworkCallRejected` → `GroupTxGranted` / `GroupTxRejected`.
//! - Downlink voice → jitter buffer → one 35-byte frame per TDMA frame in the call's slot
//!   (frame 18 carries no traffic), exactly like Brew's playout.
//! - Uplink voice of monitored groups → `VoiceDatagram::Ul` to the core.
//! - Watchdogs: max TX time, link loss → the dispatcher's transmission ends; the carrier never
//!   stays keyed for a dead console.

use std::collections::{HashMap, HashSet};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use dispatch_proto::{ACELP_FRAME_BYTES, AcelpFrame, CallPriority, CoreToNode, NodeToCore, RejectReason, StopReason, VoiceDatagram};
use tetra_config::bluestation::SharedConfig;
use tetra_core::{TdmaTime, tetra_common::Sap, tetra_entities::TetraEntity};
use tetra_saps::{
    SapMsg, SapMsgInner,
    control::call_control::{CallControl, NetworkCallRejectReason},
    tmd::{TmdCircuitDataInd, TmdCircuitDataReq},
};
use uuid::Uuid;

use super::worker::{DispatcherCommand, DispatcherEvent, DispatcherWorker, WorkerConfig};
use crate::net_brew::components::jitter_buffer::VoiceJitterBuffer;
use crate::net_telemetry::TelemetrySource;
use crate::network::transports::NetworkTransport;
use crate::{MessageQueue, TetraEntityTrait};

/// Worker events handled per TDMA tick at most (keeps the RT loop bounded).
const MAX_EVENTS_PER_TICK: usize = 64;
/// Queue depths between entity and worker.
const EVENT_QUEUE: usize = 1024;
const COMMAND_QUEUE: usize = 4096;
/// Refresh CMCE's network-call timeout about once a second while voice flows (72 slots ≈ 1 s).
const MEDIA_ACTIVITY_REFRESH_TS: i32 = 72;
/// A request CMCE never answered is dropped after this long (~5 s).
const SETUP_ANSWER_TIMEOUT_TS: i32 = 5 * 71;
/// Leftover playout after a stop is reaped after this long (~5 s).
const DRAIN_REAP_TS: i32 = 5 * 71;
/// TDMA frame 18 is the control frame: no traffic.
const CONTROL_FRAME: u8 = 18;

/// Seconds → TDMA timeslots (one slot = 170/12 ms).
fn seconds_to_ts(seconds: u32) -> i32 {
    (f64::from(seconds) * 1000.0 / (170.0 / 12.0)) as i32
}

struct TxSession {
    req_id: u32,
    gssi: u32,
    call_id: Option<u16>,
    carrier: Option<u16>,
    ts: Option<u8>,
    started: TdmaTime,
    last_activity_signal: Option<TdmaTime>,
    jitter: VoiceJitterBuffer,
}

struct Draining {
    carrier: u16,
    ts: u8,
    jitter: VoiceJitterBuffer,
    since: TdmaTime,
}

struct KnownCall {
    gssi: u32,
    speaker: u32,
    seq: u32,
}

pub struct DispatcherEntity {
    config: SharedConfig,
    dltime: TdmaTime,
    events: Receiver<DispatcherEvent>,
    commands: Sender<DispatcherCommand>,
    link_up: bool,
    dispatcher_issi: Option<u32>,
    monitored: HashSet<u32>,
    tx: HashMap<Uuid, TxSession>,
    draining: Vec<Draining>,
    /// Group calls seen via FloorGranted, for routing uplink voice: call_id → state.
    calls: HashMap<u16, KnownCall>,
    calls_by_slot: HashMap<(u16, u8), u16>,
    max_tx_ts: i32,
    worker: Option<thread::JoinHandle<()>>,
}

impl DispatcherEntity {
    /// Start the entity and its worker thread. `telemetry` feeds subscriber/call state to the core.
    pub fn new<T: NetworkTransport + 'static>(config: SharedConfig, transport: T, telemetry: Option<TelemetrySource>) -> Self {
        let cfg = config.config().dispatcher.clone().expect("DispatcherEntity requires a [dispatcher] section");
        let (event_tx, event_rx) = bounded(EVENT_QUEUE);
        let (cmd_tx, cmd_rx) = bounded(COMMAND_QUEUE);
        let worker_cfg = WorkerConfig {
            node_id: cfg.node_id.clone(),
            token: cfg.token.clone(),
            software: format!("flowstation {}", env!("CARGO_PKG_VERSION")),
            fallback_issi: cfg.fallback_issi,
            dispatch_gssis: cfg.gssis.clone(),
            link_timeout: std::time::Duration::from_millis(u64::from(cfg.link_timeout_ms)),
        };
        let handle = thread::Builder::new()
            .name("dispatcher-worker".into())
            .spawn(move || DispatcherWorker::new(worker_cfg, transport, event_tx, cmd_rx, telemetry).run())
            .expect("failed to spawn dispatcher worker thread");
        let mut entity = Self::from_channels(config, event_rx, cmd_tx);
        entity.worker = Some(handle);
        entity
    }

    /// Entity without a worker thread (tests drive the channels directly).
    pub fn from_channels(config: SharedConfig, events: Receiver<DispatcherEvent>, commands: Sender<DispatcherCommand>) -> Self {
        let (fallback, max_tx) = match &config.config().dispatcher {
            Some(d) => (d.fallback_issi, d.max_tx_seconds),
            None => (None, 60),
        };
        Self {
            config,
            dltime: TdmaTime::default(),
            events,
            commands,
            link_up: false,
            dispatcher_issi: fallback,
            monitored: HashSet::new(),
            tx: HashMap::new(),
            draining: Vec::new(),
            calls: HashMap::new(),
            calls_by_slot: HashMap::new(),
            max_tx_ts: seconds_to_ts(max_tx),
            worker: None,
        }
    }

    fn to_core(&self, msg: NodeToCore) {
        self.command(DispatcherCommand::Send(msg));
    }

    fn command(&self, cmd: DispatcherCommand) {
        if let Err(TrySendError::Full(cmd)) = self.commands.try_send(cmd) {
            tracing::warn!("Dispatcher: worker queue full, dropping {:?}", cmd);
        }
    }

    fn to_cmce(queue: &mut MessageQueue, control: CallControl) {
        queue.push_back(SapMsg {
            sap: Sap::Control,
            src: TetraEntity::Dispatcher,
            dest: TetraEntity::Cmce,
            msg: SapMsgInner::CmceCallControl(control),
        });
    }

    fn reject(&self, req_id: u32, reason: RejectReason) {
        tracing::info!("Dispatcher: TX request {} refused: {:?}", req_id, reason);
        self.to_core(NodeToCore::GroupTxRejected { req_id, reason });
    }

    // ----- core → node -----

    fn handle_event(&mut self, queue: &mut MessageQueue, event: DispatcherEvent) {
        match event {
            DispatcherEvent::LinkUp => self.link_up = true,
            DispatcherEvent::LinkDown => {
                self.link_up = false;
                self.end_all_tx(queue, StopReason::LinkLost);
                self.monitored.clear();
            }
            DispatcherEvent::DispatcherIssi(issi) => self.dispatcher_issi = issi,
            DispatcherEvent::FromCore(CoreToNode::GroupTxRequest { req_id, gssi, priority }) => {
                self.start_tx(queue, req_id, gssi, priority);
            }
            DispatcherEvent::FromCore(CoreToNode::GroupTxStop { call_id }) => self.stop_tx(queue, call_id, StopReason::Requested),
            DispatcherEvent::FromCore(CoreToNode::MonitorSet { gssis }) => self.monitored = gssis.into_iter().collect(),
            DispatcherEvent::FromCore(other) => tracing::debug!("Dispatcher: ignoring {:?}", other),
            DispatcherEvent::VoiceDl(VoiceDatagram::Dl { call_id, frame, .. }) => {
                if let Some(session) = self.tx.values_mut().find(|s| s.call_id == Some(call_id)) {
                    session.jitter.push(frame.0.to_vec());
                }
            }
            DispatcherEvent::VoiceDl(VoiceDatagram::Ul { .. }) => {}
        }
    }

    fn start_tx(&mut self, queue: &mut MessageQueue, req_id: u32, gssi: u32, priority: CallPriority) {
        if !self.link_up {
            return;
        }
        let Ok(priority) = priority.checked() else {
            return self.reject(req_id, RejectReason::NotAllowed);
        };
        let Some(issi) = self.dispatcher_issi else {
            return self.reject(req_id, RejectReason::NoDispatcherIssi);
        };
        if self.tx.values().any(|s| s.gssi == gssi) {
            return self.reject(req_id, RejectReason::FloorBusy);
        }
        let uuid = Uuid::new_v4();
        self.tx.insert(
            uuid,
            TxSession {
                req_id,
                gssi,
                call_id: None,
                carrier: None,
                ts: None,
                started: self.dltime,
                last_activity_signal: None,
                jitter: VoiceJitterBuffer::with_initial_latency(0),
            },
        );
        tracing::info!("Dispatcher: TX request {} on gssi={} prio={} as ISSI {}", req_id, gssi, priority.value(), issi);
        Self::to_cmce(
            queue,
            CallControl::NetworkCallStart { brew_uuid: uuid, source_issi: issi, dest_gssi: gssi, priority: priority.value() },
        );
    }

    /// End the dispatcher's transmission on `call_id`. The call goes to hangtime in CMCE, so
    /// radios can answer. Buffered frames still play out.
    fn stop_tx(&mut self, queue: &mut MessageQueue, call_id: u16, reason: StopReason) {
        let Some(uuid) = self.tx.iter().find(|(_, s)| s.call_id == Some(call_id)).map(|(u, _)| *u) else {
            return;
        };
        let session = self.tx.remove(&uuid).expect("session just found");
        Self::to_cmce(queue, CallControl::NetworkCallEnd { brew_uuid: uuid });
        if let (Some(carrier), Some(ts)) = (session.carrier, session.ts)
            && !session.jitter.is_empty()
        {
            self.draining.push(Draining { carrier, ts, jitter: session.jitter, since: self.dltime });
        }
        tracing::info!(
            "Dispatcher: TX on gssi={} call_id={} ended ({:?}) after {} slots",
            session.gssi,
            call_id,
            reason,
            session.started.age(self.dltime)
        );
        if reason != StopReason::Requested {
            self.to_core(NodeToCore::TxForcedStop { call_id, reason });
        }
    }

    fn end_all_tx(&mut self, queue: &mut MessageQueue, reason: StopReason) {
        let sessions: Vec<(Uuid, Option<u16>)> = self.tx.iter().map(|(u, s)| (*u, s.call_id)).collect();
        for (uuid, call_id) in sessions {
            match call_id {
                Some(call_id) => self.stop_tx(queue, call_id, reason),
                None => {
                    // Not set up yet: tell CMCE anyway so no call is left behind.
                    self.tx.remove(&uuid);
                    Self::to_cmce(queue, CallControl::NetworkCallEnd { brew_uuid: uuid });
                }
            }
        }
    }

    // ----- CMCE / UMAC → node -----

    fn rx_call_control(&mut self, queue: &mut MessageQueue, control: CallControl) {
        match control {
            CallControl::NetworkCallReady { brew_uuid, call_id, carrier_num, ts, .. } => {
                let Some(session) = self.tx.get_mut(&brew_uuid) else {
                    // Our console vanished in the meantime: release the call right away.
                    Self::to_cmce(queue, CallControl::NetworkCallEnd { brew_uuid });
                    return;
                };
                session.call_id = Some(call_id);
                session.carrier = Some(carrier_num);
                session.ts = Some(ts);
                let req_id = session.req_id;
                self.to_core(NodeToCore::GroupTxGranted { req_id, call_id, carrier: carrier_num, ts });
            }
            CallControl::NetworkCallRejected { brew_uuid, reason } => {
                if let Some(session) = self.tx.remove(&brew_uuid) {
                    self.reject(session.req_id, map_reject(reason));
                }
            }
            CallControl::NetworkCallEnd { brew_uuid } => {
                // CMCE ended our call (timeout, no listeners left, ...).
                if let Some(session) = self.tx.remove(&brew_uuid) {
                    match session.call_id {
                        Some(call_id) => self.to_core(NodeToCore::TxForcedStop { call_id, reason: StopReason::CallReleased }),
                        None => self.reject(session.req_id, RejectReason::NoResources),
                    }
                }
            }
            CallControl::FloorGranted { call_id, source_issi, dest_gssi, carrier_num, ts } => {
                let seq = self.calls.get(&call_id).map_or(0, |c| c.seq);
                self.calls.insert(call_id, KnownCall { gssi: dest_gssi, speaker: source_issi, seq });
                self.calls_by_slot.insert((carrier_num, ts), call_id);
            }
            CallControl::FloorReleased { call_id, .. } => {
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.speaker = 0;
                }
            }
            CallControl::CallEnded { call_id, carrier_num, ts } => {
                self.calls.remove(&call_id);
                if self.calls_by_slot.get(&(carrier_num, ts)) == Some(&call_id) {
                    self.calls_by_slot.remove(&(carrier_num, ts));
                }
                if let Some(uuid) = self.tx.iter().find(|(_, s)| s.call_id == Some(call_id)).map(|(u, _)| *u) {
                    self.tx.remove(&uuid);
                    self.to_core(NodeToCore::TxForcedStop { call_id, reason: StopReason::CallReleased });
                }
            }
            _ => {}
        }
    }

    fn rx_ul_voice(&mut self, prim: TmdCircuitDataInd) {
        if !self.link_up {
            return;
        }
        let Some(call_id) = self.calls_by_slot.get(&(prim.carrier_num, prim.ts)).copied() else {
            return;
        };
        let Some(call) = self.calls.get_mut(&call_id) else { return };
        if !self.monitored.contains(&call.gssi) || call.speaker == 0 {
            return;
        }
        let Some(frame) = acelp_from_umac(&prim.data) else {
            tracing::debug!("Dispatcher: unexpected UL voice length {}", prim.data.len());
            return;
        };
        call.seq = call.seq.wrapping_add(1);
        let datagram = VoiceDatagram::Ul { call_id, gssi: call.gssi, speaker_issi: call.speaker, seq: call.seq, frame };
        self.command(DispatcherCommand::Voice(datagram));
    }

    // ----- per-tick work -----

    fn watchdogs(&mut self, queue: &mut MessageQueue) {
        let now = self.dltime;
        let overdue: Vec<u16> = self
            .tx
            .values()
            .filter(|s| s.call_id.is_some() && s.started.age(now) > self.max_tx_ts)
            .filter_map(|s| s.call_id)
            .collect();
        for call_id in overdue {
            tracing::warn!("Dispatcher: TX on call_id={} hit max_tx_seconds, cutting it", call_id);
            self.stop_tx(queue, call_id, StopReason::MaxTxTime);
        }
        let unanswered: Vec<Uuid> = self
            .tx
            .iter()
            .filter(|(_, s)| s.call_id.is_none() && s.started.age(now) > SETUP_ANSWER_TIMEOUT_TS)
            .map(|(u, _)| *u)
            .collect();
        for uuid in unanswered {
            if let Some(session) = self.tx.remove(&uuid) {
                Self::to_cmce(queue, CallControl::NetworkCallEnd { brew_uuid: uuid });
                self.reject(session.req_id, RejectReason::NoResources);
            }
        }
    }

    fn playout(&mut self, queue: &mut MessageQueue) {
        if self.dltime.f == CONTROL_FRAME {
            return;
        }
        let now = self.dltime;
        let mut out: Vec<(u16, u8, Vec<u8>)> = Vec::new();
        let mut activity: Vec<Uuid> = Vec::new();
        for (uuid, session) in self.tx.iter_mut() {
            let (Some(carrier), Some(ts)) = (session.carrier, session.ts) else { continue };
            if ts != now.t {
                continue;
            }
            if let Some(frame) = session.jitter.pop_ready() {
                out.push((carrier, ts, frame.acelp_data));
                if session.last_activity_signal.is_none_or(|t| t.age(now) >= MEDIA_ACTIVITY_REFRESH_TS) {
                    session.last_activity_signal = Some(now);
                    activity.push(*uuid);
                }
            }
        }
        self.draining.retain_mut(|d| {
            if d.since.age(now) > DRAIN_REAP_TS {
                return false;
            }
            if d.ts != now.t {
                return true;
            }
            match d.jitter.pop_drain() {
                Some(frame) => {
                    out.push((d.carrier, d.ts, frame.acelp_data));
                    true
                }
                None => false,
            }
        });
        for uuid in activity {
            Self::to_cmce(queue, CallControl::NetworkCallMediaActivity { brew_uuid: uuid });
        }
        for (carrier_num, ts, data) in out {
            queue.push_back(SapMsg {
                sap: Sap::TmdSap,
                src: TetraEntity::Dispatcher,
                dest: TetraEntity::Umac,
                msg: SapMsgInner::TmdCircuitDataReq(TmdCircuitDataReq { carrier_num, ts, data }),
            });
        }
    }

    pub fn shutdown(&mut self) {
        self.command(DispatcherCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl TetraEntityTrait for DispatcherEntity {
    fn entity(&self) -> TetraEntity {
        TetraEntity::Dispatcher
    }

    fn set_config(&mut self, config: SharedConfig) {
        if let Some(d) = &config.config().dispatcher {
            self.max_tx_ts = seconds_to_ts(d.max_tx_seconds);
        }
        self.config = config;
    }

    fn rx_prim(&mut self, queue: &mut MessageQueue, message: SapMsg) {
        match message.msg {
            SapMsgInner::CmceCallControl(control) => self.rx_call_control(queue, control),
            SapMsgInner::TmdCircuitDataInd(prim) => self.rx_ul_voice(prim),
            _ => {}
        }
    }

    fn tick_start(&mut self, queue: &mut MessageQueue, ts: TdmaTime) {
        self.dltime = ts;
        for _ in 0..MAX_EVENTS_PER_TICK {
            match self.events.try_recv() {
                Ok(event) => self.handle_event(queue, event),
                Err(_) => break,
            }
        }
        self.watchdogs(queue);
        self.playout(queue);
    }
}

impl Drop for DispatcherEntity {
    fn drop(&mut self) {
        let _ = self.commands.try_send(DispatcherCommand::Shutdown);
    }
}

fn map_reject(reason: NetworkCallRejectReason) -> RejectReason {
    match reason {
        NetworkCallRejectReason::NotAllowed => RejectReason::NotAllowed,
        NetworkCallRejectReason::BrewRoutable => RejectReason::BrewRoutable,
        NetworkCallRejectReason::NoListeners => RejectReason::NoListeners,
        NetworkCallRejectReason::FloorBusy => RejectReason::FloorBusy,
        NetworkCallRejectReason::NoResources => RejectReason::NoResources,
    }
}

/// UMAC hands traffic either as 274 one-bit-per-byte values or already packed (35 bytes,
/// or 36 with a leading STE header byte) — the same three shapes Brew accepts.
fn acelp_from_umac(data: &[u8]) -> Option<AcelpFrame> {
    match data.len() {
        ACELP_FRAME_BYTES => Some(AcelpFrame(data.try_into().ok()?)),
        36 => Some(AcelpFrame(data[1..].try_into().ok()?)),
        n if n >= 274 => AcelpFrame::from_bits(data),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umac_frame_shapes() {
        assert_eq!(acelp_from_umac(&[7u8; 35]), Some(AcelpFrame([7; 35])));
        let mut ste = vec![0u8];
        ste.extend([9u8; 35]);
        assert_eq!(acelp_from_umac(&ste), Some(AcelpFrame([9; 35])));
        let bits = vec![1u8; 274];
        assert!(acelp_from_umac(&bits).is_some());
        assert_eq!(acelp_from_umac(&[0u8; 10]), None);
    }

    #[test]
    fn max_tx_conversion() {
        // 60 s ≈ 4235 slots of 14.17 ms.
        assert_eq!(seconds_to_ts(60), 4235);
    }

    #[test]
    fn reject_mapping_is_total() {
        assert_eq!(map_reject(NetworkCallRejectReason::BrewRoutable), RejectReason::BrewRoutable);
        assert_eq!(map_reject(NetworkCallRejectReason::FloorBusy), RejectReason::FloorBusy);
    }
}
