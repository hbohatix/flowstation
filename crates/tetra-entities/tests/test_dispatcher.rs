//! DispatcherEntity driven directly through its channels (no worker thread, no network).

mod common;

use crossbeam_channel::{Receiver, Sender, bounded};
use dispatch_proto::{AcelpFrame, CallPriority, CoreToNode, NodeToCore, RejectReason, StopReason, VoiceDatagram};
use tetra_config::bluestation::{CfgDispatcher, StackMode};
use tetra_core::tetra_entities::TetraEntity;
use tetra_core::{Sap, TdmaTime};
use tetra_entities::net_dispatcher::entity::DispatcherEntity;
use tetra_entities::net_dispatcher::worker::{DispatcherCommand, DispatcherEvent};
use tetra_entities::{MessageQueue, TetraEntityTrait};
use tetra_saps::control::call_control::{CallControl, NetworkCallRejectReason};
use tetra_saps::sapmsg::{SapMsg, SapMsgInner};
use tetra_saps::tmd::TmdCircuitDataInd;

use crate::common::ComponentTest;

const GSSI: u32 = 91;
const FALLBACK_ISSI: u32 = 9_000_001;

struct Harness {
    entity: DispatcherEntity,
    events: Sender<DispatcherEvent>,
    commands: Receiver<DispatcherCommand>,
    queue: MessageQueue,
    now: TdmaTime,
}

impl Harness {
    fn new(fallback_issi: Option<u32>, max_tx_seconds: u32) -> Self {
        let mut config = ComponentTest::get_default_test_config(StackMode::Bs);
        config.dispatcher = Some(CfgDispatcher {
            enabled: true,
            server_host: "127.0.0.1".into(),
            server_port: 4433,
            server_name: None,
            ca_cert: None,
            node_id: "n".into(),
            token: "t".into(),
            gssis: vec![GSSI],
            fallback_issi,
            default_priority: 0,
            max_tx_seconds,
            link_timeout_ms: 6000,
        });
        let shared = ComponentTest::from_config(config, None).get_shared_config();
        let (event_tx, event_rx) = bounded(64);
        let (cmd_tx, cmd_rx) = bounded(4096);
        let entity = DispatcherEntity::from_channels(shared, event_rx, cmd_tx);
        Self { entity, events: event_tx, commands: cmd_rx, queue: MessageQueue::new(), now: TdmaTime { h: 0, m: 1, f: 1, t: 1 } }
    }

    fn event(&mut self, ev: DispatcherEvent) {
        self.events.send(ev).unwrap();
    }

    /// Advance one timeslot and run the entity's tick.
    fn tick(&mut self) {
        self.now = self.now.add_timeslots(1);
        self.entity.tick_start(&mut self.queue, self.now);
    }

    fn ticks(&mut self, n: usize) {
        for _ in 0..n {
            self.tick();
        }
    }

    fn rx(&mut self, msg: SapMsgInner, src: TetraEntity) {
        let message = SapMsg { sap: Sap::Control, src, dest: TetraEntity::Dispatcher, msg };
        self.entity.rx_prim(&mut self.queue, message);
    }

    fn drain_queue(&mut self) -> Vec<SapMsg> {
        let mut out = Vec::new();
        while let Some(m) = self.queue.pop_front() {
            out.push(m);
        }
        out
    }

    fn sent_to_core(&self) -> Vec<NodeToCore> {
        self.commands
            .try_iter()
            .filter_map(|c| match c {
                DispatcherCommand::Send(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn voice_to_core(&self) -> Vec<VoiceDatagram> {
        self.commands
            .try_iter()
            .filter_map(|c| match c {
                DispatcherCommand::Voice(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    /// Link up + GroupTxRequest; returns the uuid CMCE was asked to start.
    fn request_tx(&mut self, req_id: u32, priority: u8) -> uuid::Uuid {
        self.event(DispatcherEvent::LinkUp);
        self.event(DispatcherEvent::FromCore(CoreToNode::GroupTxRequest {
            req_id,
            gssi: GSSI,
            priority: CallPriority::new(priority).unwrap(),
        }));
        self.tick();
        self.drain_queue()
            .iter()
            .find_map(|m| match &m.msg {
                SapMsgInner::CmceCallControl(CallControl::NetworkCallStart { brew_uuid, source_issi, dest_gssi, priority: p })
                    if m.dest == TetraEntity::Cmce =>
                {
                    assert_eq!(*dest_gssi, GSSI);
                    assert_eq!(*source_issi, FALLBACK_ISSI);
                    assert_eq!(*p, priority);
                    Some(*brew_uuid)
                }
                _ => None,
            })
            .expect("NetworkCallStart to CMCE")
    }

    fn ready(&mut self, uuid: uuid::Uuid, call_id: u16, ts: u8) {
        self.rx(
            SapMsgInner::CmceCallControl(CallControl::NetworkCallReady { brew_uuid: uuid, call_id, carrier_num: 0, ts, usage: 4 }),
            TetraEntity::Cmce,
        );
    }
}

fn ends_call(msgs: &[SapMsg], uuid: uuid::Uuid) -> bool {
    msgs.iter().any(|m| {
        m.dest == TetraEntity::Cmce
            && matches!(&m.msg, SapMsgInner::CmceCallControl(CallControl::NetworkCallEnd { brew_uuid }) if *brew_uuid == uuid)
    })
}

#[test]
fn request_is_granted_after_cmce_is_ready() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(7, 11);
    h.ready(uuid, 42, 2);
    assert!(h.sent_to_core().contains(&NodeToCore::GroupTxGranted { req_id: 7, call_id: 42, carrier: 0, ts: 2 }));
}

#[test]
fn cmce_refusal_is_reported_with_its_reason() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(8, 0);
    h.rx(
        SapMsgInner::CmceCallControl(CallControl::NetworkCallRejected { brew_uuid: uuid, reason: NetworkCallRejectReason::NoListeners }),
        TetraEntity::Cmce,
    );
    assert!(h.sent_to_core().contains(&NodeToCore::GroupTxRejected { req_id: 8, reason: RejectReason::NoListeners }));
}

#[test]
fn no_dispatcher_issi_means_refusal() {
    let mut h = Harness::new(None, 60);
    h.event(DispatcherEvent::LinkUp);
    h.event(DispatcherEvent::FromCore(CoreToNode::GroupTxRequest { req_id: 1, gssi: GSSI, priority: CallPriority::NOT_DEFINED }));
    h.tick();
    assert!(h.drain_queue().is_empty(), "nothing must reach CMCE");
    assert!(h.sent_to_core().contains(&NodeToCore::GroupTxRejected { req_id: 1, reason: RejectReason::NoDispatcherIssi }));
}

#[test]
fn setting_change_is_used_for_the_next_call() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    h.event(DispatcherEvent::DispatcherIssi(Some(9_100_000)));
    h.event(DispatcherEvent::LinkUp);
    h.event(DispatcherEvent::FromCore(CoreToNode::GroupTxRequest { req_id: 1, gssi: GSSI, priority: CallPriority::NOT_DEFINED }));
    h.tick();
    assert!(h.drain_queue().iter().any(|m| matches!(
        &m.msg,
        SapMsgInner::CmceCallControl(CallControl::NetworkCallStart { source_issi: 9_100_000, .. })
    )));
}

#[test]
fn downlink_voice_plays_in_the_call_slot_and_skips_frame_18() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(1, 0);
    let call_ts = 2;
    h.ready(uuid, 42, call_ts);
    for seq in 0..40 {
        h.event(DispatcherEvent::VoiceDl(VoiceDatagram::Dl { call_id: 42, seq, frame: AcelpFrame([seq as u8; 35]) }));
    }
    let mut played = Vec::new();
    for _ in 0..(4 * 18 * 2) {
        h.tick();
        for m in h.drain_queue() {
            if let SapMsgInner::TmdCircuitDataReq(req) = &m.msg {
                assert_eq!(m.dest, TetraEntity::Umac);
                assert_eq!(req.ts, call_ts, "voice only in the call's timeslot");
                assert_ne!(h.now.f, 18, "frame 18 carries no traffic");
                assert_eq!(req.data.len(), 35);
                played.push(req.data[0]);
            }
        }
    }
    assert!(played.len() >= 20, "frames were played out: {}", played.len());
    assert!(played.windows(2).all(|w| w[0] < w[1]), "in order: {played:?}");
}

#[test]
fn link_loss_ends_the_transmission() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(1, 0);
    h.ready(uuid, 42, 2);
    h.event(DispatcherEvent::LinkDown);
    h.tick();
    assert!(ends_call(&h.drain_queue(), uuid), "carrier must not stay keyed for a dead console");
}

#[test]
fn max_tx_time_cuts_the_transmission() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 5);
    let uuid = h.request_tx(1, 0);
    h.ready(uuid, 42, 2);
    h.drain_queue();
    h.ticks(5 * 71 + 10);
    assert!(ends_call(&h.drain_queue(), uuid));
    assert!(h.sent_to_core().contains(&NodeToCore::TxForcedStop { call_id: 42, reason: StopReason::MaxTxTime }));
}

#[test]
fn ptt_release_ends_the_call_leg_without_forced_stop() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(1, 0);
    h.ready(uuid, 42, 2);
    h.sent_to_core();
    h.event(DispatcherEvent::FromCore(CoreToNode::GroupTxStop { call_id: 42 }));
    h.tick();
    assert!(ends_call(&h.drain_queue(), uuid));
    assert!(!h.sent_to_core().iter().any(|m| matches!(m, NodeToCore::TxForcedStop { .. })));
}

#[test]
fn second_request_on_same_group_is_busy() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let _ = h.request_tx(1, 0);
    h.event(DispatcherEvent::FromCore(CoreToNode::GroupTxRequest { req_id: 2, gssi: GSSI, priority: CallPriority::NOT_DEFINED }));
    h.tick();
    assert!(h.sent_to_core().contains(&NodeToCore::GroupTxRejected { req_id: 2, reason: RejectReason::FloorBusy }));
}

#[test]
fn uplink_of_monitored_group_is_sent_with_speaker() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    h.event(DispatcherEvent::LinkUp);
    h.event(DispatcherEvent::FromCore(CoreToNode::MonitorSet { all: false, gssis: vec![GSSI] }));
    h.tick();
    h.rx(
        SapMsgInner::CmceCallControl(CallControl::FloorGranted { call_id: 5, source_issi: 2_603_877, dest_gssi: GSSI, carrier_num: 0, ts: 3 }),
        TetraEntity::Cmce,
    );
    h.rx(SapMsgInner::TmdCircuitDataInd(TmdCircuitDataInd { carrier_num: 0, ts: 3, data: vec![0xAB; 35] }), TetraEntity::Umac);
    let voice = h.voice_to_core();
    assert_eq!(
        voice,
        vec![VoiceDatagram::Ul { call_id: 5, gssi: GSSI, speaker_issi: 2_603_877, seq: 1, frame: AcelpFrame([0xAB; 35]) }]
    );
}

#[test]
fn uplink_of_unmonitored_group_is_not_sent() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    h.event(DispatcherEvent::LinkUp);
    h.tick();
    h.rx(
        SapMsgInner::CmceCallControl(CallControl::FloorGranted { call_id: 5, source_issi: 7, dest_gssi: 220, carrier_num: 0, ts: 3 }),
        TetraEntity::Cmce,
    );
    h.rx(SapMsgInner::TmdCircuitDataInd(TmdCircuitDataInd { carrier_num: 0, ts: 3, data: vec![1; 35] }), TetraEntity::Umac);
    assert!(h.voice_to_core().is_empty());
}

#[test]
fn ptt_release_plays_out_the_buffer_before_hangtime() {
    // UMAC sends no traffic in hangtime: ending the call while frames are still buffered would
    // clip the end of the dispatcher's sentence. Release must play them out first.
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(1, 0);
    h.ready(uuid, 42, 2);
    for seq in 0..8 {
        h.event(DispatcherEvent::VoiceDl(VoiceDatagram::Dl { call_id: 42, seq, frame: AcelpFrame([seq as u8 + 1; 35]) }));
    }
    // Release right away, before the jitter buffer even started playing.
    h.event(DispatcherEvent::FromCore(CoreToNode::GroupTxStop { call_id: 42 }));
    let mut played = Vec::new();
    let mut ended_after = None;
    for _ in 0..(4 * 18 * 2) {
        h.tick();
        for m in h.drain_queue() {
            match &m.msg {
                SapMsgInner::TmdCircuitDataReq(req) => {
                    assert!(ended_after.is_none(), "no voice after the call went to hangtime");
                    played.push(req.data[0]);
                }
                SapMsgInner::CmceCallControl(CallControl::NetworkCallEnd { brew_uuid }) if *brew_uuid == uuid => {
                    ended_after = Some(played.len());
                }
                _ => {}
            }
        }
    }
    assert_eq!(played, (1..=8).collect::<Vec<u8>>(), "every buffered frame reaches the air, in order");
    assert_eq!(ended_after, Some(8), "the call ends only after the last frame");
    assert!(!h.sent_to_core().iter().any(|m| matches!(m, NodeToCore::TxForcedStop { .. })));
}

#[test]
fn link_loss_does_not_wait_for_the_buffer() {
    let mut h = Harness::new(Some(FALLBACK_ISSI), 60);
    let uuid = h.request_tx(1, 0);
    h.ready(uuid, 42, 2);
    for seq in 0..8 {
        h.event(DispatcherEvent::VoiceDl(VoiceDatagram::Dl { call_id: 42, seq, frame: AcelpFrame([1; 35]) }));
    }
    h.event(DispatcherEvent::LinkDown);
    h.tick();
    assert!(ends_call(&h.drain_queue(), uuid), "a dead console ends the transmission at once");
}
