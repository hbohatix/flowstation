//! Dispatcher link to the flowstation-platform core.
//!
//! The node connects out to the core over QUIC/TLS (see `crates/dispatch-proto` for the wire
//! format, `Docs/dispatcher-link.md` for the design). Dispatcher calls are local to this cell and
//! never reach Brew — see [`routing`].
//!
//! - [`entity::DispatcherEntity`] runs in the real-time loop: talks to CMCE/UMAC via `SapMsg`,
//!   plays downlink voice out per TDMA frame, enforces the TX watchdogs. No I/O.
//! - [`worker::DispatcherWorker`] runs on its own thread: connect, `Hello`, heartbeats,
//!   reconnect, settings, subscriber state from telemetry.

pub mod entity;
pub mod routing;
pub mod worker;

use std::time::Duration;

use tetra_config::bluestation::CfgDispatcher;

use crate::network::transports::quic::{QuicClientOptions, QuicTransport, QuicTrust};
use crate::network::transports::{NetworkAddress, NetworkError};

/// How long the worker waits for the core before giving up a connect attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the QUIC transport for the `[dispatcher]` section (pinned certificate when `ca_cert` is set).
pub fn new_quic_transport(cfg: &CfgDispatcher) -> Result<QuicTransport, NetworkError> {
    let trust = match &cfg.ca_cert {
        Some(path) => QuicTrust::PinnedPem(
            std::fs::read(path)
                .map_err(|e| NetworkError::ConnectionFailed(format!("dispatcher: cannot read ca_cert {path}: {e}")))?,
        ),
        None => QuicTrust::System,
    };
    let options = QuicClientOptions {
        alpn: vec![dispatch_proto::ALPN.to_vec()],
        trust,
        server_name: cfg.server_name.clone(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("dispatcher-quic")
        .enable_all()
        .build()
        .map_err(|e| NetworkError::ConnectionFailed(format!("dispatcher: tokio runtime: {e}")))?;
    QuicTransport::with_options(
        NetworkAddress::Udp { host: cfg.server_host.clone(), port: cfg.server_port },
        CONNECT_TIMEOUT,
        options,
        runtime,
    )
}
