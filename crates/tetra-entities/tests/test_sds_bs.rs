mod common;

use std::time::Duration;

use tetra_config::bluestation::{CfgBrew, StackMode};
use tetra_core::tetra_entities::TetraEntity;
use tetra_core::{BitBuffer, Sap, SsiType, TdmaTime, TetraAddress, debug};
use tetra_pdus::cmce::enums::party_type_identifier::PartyTypeIdentifier;
use tetra_pdus::cmce::enums::pre_coded_status::PreCodedStatus;
use tetra_pdus::cmce::fields::basic_service_information::BasicServiceInformation;
use tetra_pdus::cmce::pdus::u_sds_data::USdsData;
use tetra_pdus::cmce::enums::disconnect_cause::DisconnectCause;
use tetra_pdus::cmce::pdus::u_disconnect::UDisconnect;
use tetra_pdus::cmce::pdus::u_setup::USetup;
use tetra_pdus::cmce::pdus::u_status::UStatus;
use tetra_saps::control::brew::{BrewSubscriberAction, MmSubscriberUpdate};
use tetra_saps::control::enums::circuit_mode_type::CircuitModeType;
use tetra_saps::control::enums::communication_type::CommunicationType;
use tetra_saps::control::enums::sds_user_data::SdsUserData;
use tetra_saps::control::sds::CmceSdsData;
use tetra_saps::lcmc::{LcmcMleUnitdataInd, LcmcMleUnitdataReq};
use tetra_saps::sapmsg::{SapMsg, SapMsgInner};

use crate::common::ComponentTest;

/// Helper: register a subscriber ISSI in the StackState subscriber registry
fn register_subscriber(test: &mut ComponentTest, issi: u32) {
    test.config.state_write().subscribers.register(issi);
}

/// Helper: affiliate a subscriber with a GSSI in the StackState subscriber registry
fn affiliate_subscriber(test: &mut ComponentTest, issi: u32, gssi: u32) {
    test.config.state_write().subscribers.affiliate(issi, gssi);
}

/// Helper: build a U-SDS-DATA message from a source ISSI to a dest SSI with 16-bit payload
fn build_u_sds_data_msg(source_issi: u32, dest_ssi: u32, payload: u16) -> SapMsg {
    let u_sds = USdsData {
        area_selection: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_short_number_address: None,
        called_party_ssi: Some(dest_ssi as u64),
        called_party_extension: None,
        user_defined_data: SdsUserData::Type1(payload),
        external_subscriber_number: None,
        dm_ms_address: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_sds.to_bitbuf(&mut sdu).expect("Failed to serialize U-SDS-DATA");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(source_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

/// Count D-SDS-DATA messages (LcmcMleUnitdataReq to Mle) in sink output
fn count_d_sds_data(msgs: &[SapMsg]) -> usize {
    msgs.iter()
        .filter(|m| m.dest == TetraEntity::Mle && matches!(&m.msg, SapMsgInner::LcmcMleUnitdataReq(_)))
        .count()
}

/// Count CmceSdsData messages to Brew in sink output
fn count_brew_sds(msgs: &[SapMsg]) -> usize {
    msgs.iter()
        .filter(|m| m.dest == TetraEntity::Brew && matches!(&m.msg, SapMsgInner::CmceSdsData(_)))
        .count()
}

#[test]
fn test_sds_local_delivery() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Register dest ISSI in StackState
    register_subscriber(&mut test, 2000001);

    // Send U-SDS-DATA from source ISSI to registered dest ISSI
    let msg = build_u_sds_data_msg(1000001, 2000001, 0xABCD);
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 1, "Expected 1 D-SDS-DATA at Mle sink for local delivery");

    // Verify the address is ISSI
    for m in &sink_msgs {
        if m.dest == TetraEntity::Mle
            && let SapMsgInner::LcmcMleUnitdataReq(ref prim) = m.msg {
                assert_eq!(prim.main_address.ssi, 2000001);
                assert_eq!(prim.main_address.ssi_type, SsiType::Issi);
            }
    }
}

#[test]
fn test_sds_brew_forward() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut config = ComponentTest::get_default_test_config(StackMode::Bs);
    config.brew = Some(CfgBrew {
        host: "test.local".into(),
        port: 3000,
        tls: false,
        username: None,
        password: None,
        reconnect_delay: Duration::from_secs(1),
        jitter_initial_latency_frames: 0,
        feature_sds_enabled: true,
        feature_rssi_export: false,
        whitelisted_ssis: None,
    });
    let mut test = ComponentTest::from_config(config, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Do NOT register dest ISSI — should forward to Brew
    let msg = build_u_sds_data_msg(1000001, 5000001, 0x1234);
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let brew_count = count_brew_sds(&sink_msgs);
    assert!(brew_count > 0, "Expected CmceSdsData at Brew sink for non-local ISSI");

    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 0, "Should not deliver locally when dest is not registered");
}

#[test]
fn test_sds_from_brew_to_local() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Register dest ISSI in StackState
    register_subscriber(&mut test, 2000001);

    // Submit CmceSdsData from Brew on Control SAP
    let msg = SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceSdsData(CmceSdsData {
            source_issi: 3000001,
            dest_issi: 2000001,
            user_defined_data: SdsUserData::Type1(0xCAFE),
        }),
    };
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 1, "Expected D-SDS-DATA at Mle sink from Brew");
}

#[test]
fn test_sds_from_brew_unregistered() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Do NOT register dest ISSI
    let msg = SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceSdsData(CmceSdsData {
            source_issi: 3000001,
            dest_issi: 9999999,
            user_defined_data: SdsUserData::Type1(0xDEAD),
        }),
    };
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 0, "Should not deliver D-SDS-DATA when dest is not registered");
}

#[test]
fn test_sds_group_delivery() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    let gssi = 100;

    // Register 3 ISSIs and affiliate them with the GSSI in StackState
    for issi in [1000001, 1000002, 1000003] {
        register_subscriber(&mut test, issi);
        affiliate_subscriber(&mut test, issi, gssi);
    }

    // Send U-SDS-DATA to the GSSI
    let msg = build_u_sds_data_msg(1000001, gssi, 0xBEEF);
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 1, "Expected exactly 1 GSSI-addressed D-SDS-DATA (not per-member)");

    // Verify the address is GSSI
    for m in &sink_msgs {
        if m.dest == TetraEntity::Mle
            && let SapMsgInner::LcmcMleUnitdataReq(ref prim) = m.msg {
                assert_eq!(prim.main_address.ssi, gssi);
                assert_eq!(prim.main_address.ssi_type, SsiType::Gssi);
            }
    }
}

#[test]
fn test_u_status_forwarded_as_d_status() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Register both source and dest
    register_subscriber(&mut test, 1000001);
    register_subscriber(&mut test, 2000001);

    // Build a U-STATUS PDU from 1000001 to 2000001 with pre-coded status 0x8210
    let u_status = UStatus {
        area_selection: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_short_number_address: None,
        called_party_ssi: Some(2000001),
        called_party_extension: None,
        pre_coded_status: PreCodedStatus::from(0x8210),
        external_subscriber_number: None,
        dm_ms_address: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_status.to_bitbuf(&mut sdu).expect("Failed to serialize U-STATUS");
    sdu.seek(0);

    let msg = SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(1000001, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    };
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();

    // Should produce exactly 1 D-STATUS at Mle sink
    let mle_msgs: Vec<_> = sink_msgs
        .iter()
        .filter(|m| m.dest == TetraEntity::Mle && matches!(&m.msg, SapMsgInner::LcmcMleUnitdataReq(_)))
        .collect();
    assert_eq!(mle_msgs.len(), 1, "Expected 1 D-STATUS at Mle sink");

    // Verify addressed to 2000001
    if let SapMsgInner::LcmcMleUnitdataReq(ref prim) = mle_msgs[0].msg {
        assert_eq!(prim.main_address.ssi, 2000001);
        assert_eq!(prim.main_address.ssi_type, SsiType::Issi);
    }
}

#[test]
fn test_u_status_brew_forward() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut config = ComponentTest::get_default_test_config(StackMode::Bs);
    config.brew = Some(CfgBrew {
        host: "test.local".into(),
        port: 3000,
        tls: false,
        username: None,
        password: None,
        reconnect_delay: Duration::from_secs(1),
        jitter_initial_latency_frames: 0,
        feature_sds_enabled: true,
        feature_rssi_export: false,
        whitelisted_ssis: None,
    });
    let mut test = ComponentTest::from_config(config, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Only register source, NOT dest — should forward to Brew
    register_subscriber(&mut test, 1000001);

    let u_status = UStatus {
        area_selection: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_short_number_address: None,
        called_party_ssi: Some(5000001),
        called_party_extension: None,
        pre_coded_status: PreCodedStatus::from(0x8210),
        external_subscriber_number: None,
        dm_ms_address: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_status.to_bitbuf(&mut sdu).expect("Failed to serialize U-STATUS");
    sdu.seek(0);

    let msg = SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(1000001, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    };
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();

    // Should forward to Brew as CmceSdsData with Type1 payload
    let brew_count = count_brew_sds(&sink_msgs);
    assert_eq!(brew_count, 1, "Expected 1 CmceSdsData at Brew sink for U-STATUS");

    // Verify the payload is Type1 with the original pre-coded status value
    let brew_msg = sink_msgs.iter().find(|m| m.dest == TetraEntity::Brew).unwrap();
    if let SapMsgInner::CmceSdsData(ref sds) = brew_msg.msg {
        assert_eq!(sds.source_issi, 1000001);
        assert_eq!(sds.dest_issi, 5000001);
        assert_eq!(sds.user_defined_data, SdsUserData::Type1(0x8210));
    } else {
        panic!("Expected CmceSdsData message at Brew sink");
    }

    // Should NOT deliver locally
    let d_sds_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_sds_count, 0, "Should not deliver locally when dest is not registered");
}

#[test]
fn test_u_status_unregistered_dest_dropped() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    // Only register source, NOT dest
    register_subscriber(&mut test, 1000001);

    let u_status = UStatus {
        area_selection: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_short_number_address: None,
        called_party_ssi: Some(9999999),
        called_party_extension: None,
        pre_coded_status: PreCodedStatus::from(0x8210),
        external_subscriber_number: None,
        dm_ms_address: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_status.to_bitbuf(&mut sdu).expect("Failed to serialize U-STATUS");
    sdu.seek(0);

    let msg = SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(1000001, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    };
    test.submit_message(msg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let d_status_count = count_d_sds_data(&sink_msgs);
    assert_eq!(d_status_count, 0, "Should not deliver D-STATUS when dest is not registered");
}

/// Build a U-SETUP for a group call from `calling_issi` to `dest_gssi`.
fn build_u_setup_group_msg(calling_issi: u32, dest_gssi: u32) -> SapMsg {
    let u_setup = USetup {
        area_selection: 0,
        hook_method_selection: false,
        simplex_duplex_selection: false,
        basic_service_information: BasicServiceInformation {
            circuit_mode_type: CircuitModeType::TchS,
            encryption_flag: false,
            communication_type: CommunicationType::P2Mp,
            slots_per_frame: None,
            speech_service: Some(0),
        },
        request_to_transmit_send_data: false,
        call_priority: 0,
        clir_control: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_ssi: Some(dest_gssi as u64),
        called_party_short_number_address: None,
        called_party_extension: None,
        external_subscriber_number: None,
        facility: None,
        dm_ms_address: None,
        proprietary: None,
    };
    let mut sdu = BitBuffer::new_autoexpand(80);
    u_setup.to_bitbuf(&mut sdu).expect("Failed to serialize USetup");
    sdu.seek(0);
    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

/// Build a U-DISCONNECT for `call_id` from the call owner, to release a group call.
fn build_u_disconnect_msg(owner_issi: u32, call_id: u16) -> SapMsg {
    let pdu = UDisconnect {
        call_identifier: call_id,
        disconnect_cause: DisconnectCause::UserRequestedDisconnection,
        facility: None,
        proprietary: None,
    };
    let mut sdu = BitBuffer::new_autoexpand(48);
    pdu.to_bitbuf(&mut sdu).expect("Failed to serialize UDisconnect");
    sdu.seek(0);
    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(owner_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

/// Count D-SDS-DATA (LcmcMleUnitdataReq to Mle) addressed to a specific ISSI.
fn d_sds_to(msgs: &[SapMsg], issi: u32) -> Vec<&LcmcMleUnitdataReq> {
    msgs.iter()
        .filter_map(|m| match &m.msg {
            SapMsgInner::LcmcMleUnitdataReq(p) if m.dest == TetraEntity::Mle && p.main_address.ssi == issi => Some(p),
            _ => None,
        })
        .collect()
}

/// FH-BUG-034 (final): an SDS to an MS engaged in a call must NOT be FACCH-stolen (a real SDS
/// exceeds one stolen half-slot, and MS firmware does not reassemble MAC fragments over STCH, so
/// it is never received and the sender's TL-ACK times out → "message failed"). Instead it is
/// DEFERRED while the destination is in the call and delivered on the MCCH — the normal, working
/// idle-MS path — as soon as the call releases.
///
/// This test drives the exact field scenario (ISSI talker of a group call) and asserts:
///   (1) while the talker is in the call, the SDS is deferred (NOTHING addressed to it is emitted);
///   (2) after the call releases, the SDS is delivered on the MCCH (no stealing_permission/chan_alloc).
#[test]
fn test_sds_to_in_call_ms_is_deferred_then_delivered_on_mcch() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    let talker = 1000001;
    let listener = 1000002;
    let gssi = 100;

    // A listener affiliated via the MM control path so the BS accepts the group call.
    for action in [BrewSubscriberAction::Register, BrewSubscriberAction::Affiliate] {
        test.submit_message(SapMsg {
            sap: Sap::Control,
            src: TetraEntity::Mm,
            dest: TetraEntity::Cmce,
            msg: SapMsgInner::MmSubscriberUpdate(MmSubscriberUpdate { issi: listener, groups: vec![gssi], action }),
        });
        test.run_stack(Some(1));
    }
    // Talker registered for SDS routing (local delivery), keys up the group call (call_id starts at 4).
    register_subscriber(&mut test, talker);
    test.submit_message(build_u_setup_group_msg(talker, gssi));
    test.run_stack(Some(2));
    test.dump_sinks();

    // Sanity: the talker is on a traffic timeslot.
    assert!(
        test.config.state_read().active_call_ts.contains_key(&talker),
        "talker should be mapped onto a traffic timeslot while in the call"
    );

    // (1) SDS to the in-call talker -> deferred: nothing addressed to the talker is emitted.
    test.submit_message(build_u_sds_data_msg(2000002, talker, 0x1234));
    test.run_stack(Some(2));
    let during = test.dump_sinks();
    assert!(
        d_sds_to(&during, talker).is_empty(),
        "SDS to an in-call MS must be deferred, not transmitted (no stealing) while the call is up"
    );

    // (2) Release the call (owner disconnects). The talker leaves active_call_ts and the deferred
    // SDS is flushed onto the MCCH.
    test.submit_message(build_u_disconnect_msg(talker, 4));
    test.run_stack(Some(3));
    assert!(
        !test.config.state_read().active_call_ts.contains_key(&talker),
        "after release the talker must no longer be on a traffic timeslot"
    );
    let after = test.dump_sinks();
    let delivered = d_sds_to(&after, talker);
    assert!(!delivered.is_empty(), "deferred SDS must be delivered once the call releases");
    assert!(
        delivered.iter().all(|p| !p.stealing_permission && p.chan_alloc.is_none()),
        "deferred SDS must be delivered on the MCCH (no FACCH stealing) after the call ends"
    );
}
