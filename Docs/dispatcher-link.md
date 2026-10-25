# Dispatcher link

**English** · [Українська](uk/dispatcher-link.md)

Connects this FlowStation out to a [flowstation-platform](https://github.com/hbohatix/flowstation-platform) core over QUIC/TLS. Through the link a dispatcher console can:

- **call a group** — the station sets up a group call on air and plays the dispatcher's voice;
  an emergency call (priority 15) takes over a lower-priority call on the group;
- **monitor and record every group call** — local speakers and network (Brew/SIP) speakers alike,
  receive only;
- **see the network** — registrations, radio groups, positions (LIP), RSSI, calls, speakers,
  emergencies, and the station's own status (uptime, carriers, load, temperature) every 10 s;
- **control radios** — DGNA, SDS, kick, re-register request, position request, network-wide bans.

The station carries ACELP frames only (35 bytes per 60 ms). The codec lives in the platform.
Protocol version 2 (copy in `crates/dispatch-proto`).

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
token = "change-me"                     # issued by the console (System → Base stations)
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

The console's **System → Base stations** page registers the station, shows its token once,
generates this section and offers the core certificate (`core-cert.pem`) for `ca_cert`.

With `[dispatcher]` enabled, restart recovery (`[recovery]`) is always on: known radios and their
groups are kept in `recovery_cache.json` next to the config.

## Behaviour

| Situation | What the station does |
|---|---|
| Dispatcher presses PTT on an allowed group with listeners | group call on air; calling party = dispatcher ISSI; D-SETUP carries the requested priority |
| PTT released | call goes to hangtime, radios can answer; their voice is monitored |
| A radio is talking | refusal `FloorBusy` (interruption per EN 300 392-2 §14.5.1.2.1 f is stage 1) |
| Group not in `gssis` / no free slot | refusal `NotAllowed` / `NoResources` |
| No radio known to listen on the group (e.g. right after a restart) | the call goes on air anyway: radios on the group hear the D-SETUP; the call is not dropped when the last known listener leaves |
| Emergency (or any pre-emptive, 12–15) PTT while a lower-priority call runs on the group | that call is released (`PreEmptiveUseOfResource`) and a new call with the dispatcher's priority is set up (EN 300 392-2 §14.8) |
| Station restart | restart recovery is always on when `[dispatcher]` is enabled: known radios and their groups are kept in `recovery_cache.json` and commanded to re-register (D-LOCATION UPDATE COMMAND with group report, EN 300 392-2 §16.4) |
| No dispatcher ISSI (setting empty, no fallback) | refusal `NoDispatcherIssi` |
| Console ISSI equals a registered radio's ISSI | station keeps its fallback and reports the conflict to the console |
| Link to the core lost | any dispatcher transmission ends within `link_timeout_ms`; reconnect with backoff |
| Transmission longer than `max_tx_seconds` | cut off, `TxForcedStop` to the console |
| Link (re)established, or `ResyncState` from the core | sends the full state: `MsRegistered`, `MsGroups` and last `MsPosition` of every registered radio (`ResyncState` also repeats `SettingsApplied`) |
| Console: DGNA / SDS (protocol v1) | executed through the CMCE control channel (same as the dashboard), answered with `CommandResult`; the radio's DGNA answer follows as `DgnaStatus` |

| Console: kick / re-register / position request (protocol v2) | CMCE `KickMs`; MM D-LOCATION UPDATE COMMAND with group report; SDS type 4 carrying a LIP IMMEDIATE LOCATION REPORT REQUEST (TS 100 392-18-1 §6.2.16) |
| Ban list in `Settings` (v2) | a banned ISSI cannot register or attach groups; registered banned radios are kicked |
| `MonitorSet { all }` (v2) | every group call is monitored, network (Brew, SIP) speakers included — receive only |
| `BackhaulStatus` (v3) | with every `NodeStatus`: is a `[brew]` section configured, is Brew connected (`network_connected`) — shown as the station's Brew state in the console |
| Every 10 s | `NodeStatus`: uptime, carriers, MCC/MNC/LA, registered radios, calls, timeslots, CPU temperature |

Without a `[dispatcher]` section FlowStation behaves exactly as before.

Changes of the dispatcher link: [dispatcher-changelog.md](dispatcher-changelog.md).

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
