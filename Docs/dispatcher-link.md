# Dispatcher link

**English** · [Українська](uk/dispatcher-link.md)

Connects this FlowStation out to a [flowstation-platform](https://github.com/hbohatix/flowstation-platform) core over QUIC/TLS. Through the link a dispatcher console can:

- **call a group** — the station sets up a group call on air and plays the dispatcher's voice;
- **monitor groups** — uplink voice of selected groups goes to the core (for listening and recording);
- **see the network** — registrations, radio groups, calls, speakers, emergencies.

The station carries ACELP frames only (35 bytes per 60 ms). The codec lives in the console.

## Safety: dispatcher traffic never reaches Brew

Dispatcher calls are local to this cell. Three independent layers keep them away from Brew/TetraPACK/BrandMeister:

1. **Startup** — if any `[dispatcher].gssis` group is routable to Brew, the dispatcher link is not started (the station keeps running) and the log says which groups conflict.
2. **Every request** — the group is checked again when the dispatcher asks to transmit (the config can be edited live); a Brew-routable group is refused with `BrewRoutable`.
3. **CMCE** — no call on a dispatch group is ever announced to Brew, whoever started it (dispatcher, a local radio, a radio answering the dispatcher). Brew therefore also drops that call's uplink voice.

A permanent test runs a full dispatcher call with Brew configured and asserts that not a single message reaches Brew (`test_dispatcher_call_never_reaches_brew`).

## Configuration

```toml
[dispatcher]
server = "192.168.1.10:4433"            # core QUIC address (host:port)
server_name = "core"                    # name in the core certificate (default: host)
ca_cert = "/etc/flowstation/core-cert.pem"  # pin the core's certificate (PEM); omit = system CAs
node_id = "station-01"
token = "change-me"                     # must match FS_NODE_TOKENS on the core
gssis = [91]                            # groups the dispatcher may call — local-only
fallback_issi = 9000001                 # used while the console setting is empty
default_priority = 0                    # EN 300 392-2 table 14.46
max_tx_seconds = 60                     # cut a dispatcher transmission after this long
link_timeout_ms = 6000                  # end dispatcher TX this soon after the link dies
```

Make the dispatch groups local-only in `[cell_info]`:

```toml
local_ssi_ranges = [[91, 91]]
```

The core's self-signed certificate is `/data/tls/core-cert.pem` in its `core-tls` volume — copy it to the station and point `ca_cert` at it.

## Behaviour

| Situation | What the station does |
|---|---|
| Dispatcher presses PTT on an allowed group with listeners | group call on air; calling party = dispatcher ISSI; D-SETUP carries the requested priority |
| PTT released | call goes to hangtime, radios can answer; their voice is monitored |
| A radio is talking | refusal `FloorBusy` (interruption per EN 300 392-2 §14.5.1.2.1 f is stage 1) |
| Group not in `gssis` / no radio listening / no free slot | refusal `NotAllowed` / `NoListeners` / `NoResources` |
| No dispatcher ISSI (setting empty, no fallback) | refusal `NoDispatcherIssi` |
| Console ISSI equals a registered radio's ISSI | station keeps its fallback and reports the conflict to the console |
| Link to the core lost | any dispatcher transmission ends within `link_timeout_ms`; reconnect with backoff |
| Transmission longer than `max_tx_seconds` | cut off, `TxForcedStop` to the console |

Without a `[dispatcher]` section FlowStation behaves exactly as before.

## Code map

| Piece | Where |
|---|---|
| Wire protocol (copy of flowstation-platform `crates/proto`) | `crates/dispatch-proto` |
| Config section and validation | `crates/tetra-config/src/bluestation/sec_dispatcher.rs` |
| Routing rules / Brew guard | `crates/tetra-entities/src/net_dispatcher/routing.rs` |
| Real-time entity (calls, playout, watchdogs, monitoring) | `crates/tetra-entities/src/net_dispatcher/entity.rs` |
| Worker thread (connect, Hello, heartbeat, settings, telemetry) | `crates/tetra-entities/src/net_dispatcher/worker.rs` |
| CMCE call owner, refusals, priority | `crates/tetra-entities/src/cmce/subentities/cc_bs/` (`CallOrigin::Network { owner }`, `notify_network_call_rejected`) |
| QUIC options (ALPN, pinned certificate, server name) | `crates/tetra-entities/src/network/transports/quic.rs` |

Design and on-air test plan: flowstation-platform `docs/plans/stage0-node-link.md`.
