# Dispatcher link — changelog

**English** · [Українська](uk/dispatcher-changelog.md)

Changes of the `[dispatcher]` link in this fork (branch `feature/dispatcher-link`). The platform
side: [flowstation-platform CHANGELOG](https://github.com/hbohatix/flowstation-platform/blob/main/CHANGELOG.md).

## Documentation — 2026-10-26

- New step-by-step setup: [dispatcher-setup.md](dispatcher-setup.md) (build, register in the console, configure, start and check, switch off, troubleshooting by log line).
- [dispatcher-link.md](dispatcher-link.md): protocol v5 behaviour (statuses, delivery reports, LIP reasons, alarm acknowledgement, emergency end); config example fixed — `gssis = [10]` inside `local_ssi_ranges = [[0, 90]]` (the old `[91]` example would keep the link off), `server_name = "localhost"`, `local_groups`.
- `example_config/config.toml`: the same example fix.
- README: a *Dispatcher console* section with the links.

## Protocol v5: statuses, LIP reasons, SDS delivery reports — 2026-10-26 (platform 0.8.0)

- SDS and statuses addressed to the dispatcher ISSI are terminated on the station (never Brew):
  the station sends the SDS-TL delivery report the radio asks for (EN 300 392-2 §29.4.2.2 —
  delivery status 0 "received" / 2 "consumed") and closes a status with the D-STATUS echo
  (§14.7.1.11; not for Emergency). The dispatcher ISSI is kept in the stack state for CMCE.
- Every pre-coded status goes to the platform (`StatusReceived`), the console can send one
  (`SendStatus` → D-STATUS to an ISSI or a GSSI).
- LIP: every report goes to the platform as `MsLocation` with its reason for sending; reports
  with "position error not known" (TS 100 392-18-1 table 6.1 note 4), long reports with "no
  shape" (now decoded) and 0/0 carry no position — and no longer put the radio at 0°/0° on the
  station dashboard either.
- `AckEmergency`: the console acknowledges an emergency alarm — D-STATUS Emergency from the
  alarm's destination address to the radio, which ends its *console acknowledge* retries.
- `EmergencyEnded { by }`: radio (normal status), timeout or operator, so the console does not
  report a timeout as a cancellation by the radio.

## Dispatcher on every local group, protocol v4, no call drop on group reports — 2026-10-25 (platform 0.7.0)

- `[dispatcher] local_groups` (default `true`): the dispatcher may transmit on every local-only
  group (`cell.local_ssi_ranges`; every group when `[brew]` is absent), not only the listed
  `gssis`. Brew-routable groups are still refused (`gate_call`), and dispatch groups are still
  never announced to Brew.
- Protocol v4: `StationInfo` with every `NodeStatus` — repeater ID (`[brew]` username), Brew host,
  colour code, neighbour cells, and the dispatch ranges the core routes dispatcher PTT by. Needs
  platform core 0.7.0 (which accepts v2–v4).
- CMCE no longer drops a live call when a radio's location update with a group report arrives as
  "deaffiliate all + affiliate reported": a group must stay without listeners for one multiframe
  (about 1 s) before its calls are dropped. Before, the radio went silent mid-call while the
  console still heard it (e.g. after the D-LOCATION UPDATE COMMAND sent on every Brew reconnect).

## PHY: recovery from a stalled TX timeline — 2026-10-25

- After an SDR/USB hiccup (RX buffer overrun) the RX side could keep a backlog of buffered
  samples and read it at exactly the real-time rate forever. The modulator gets new slot bits
  only once per RX slot, so every TX block came too late and was skipped: the cell stopped
  transmitting ("No service") until the station was restarted (seen 2026-10-25, 90 000 warnings
  in 3.5 min).
- New `phy/components/tx_watchdog.rs`: when TX has produced nothing for 0.5 s while late, the
  PHY drops the RX backlog and continues from the current SDR clock (at most once per 2 s). The
  warning names how far RX was behind. About 100 ms of uplink is lost once instead of the cell.
- "Too late to produce TX block" is logged once per 5 s with the number of late blocks instead
  of once per block (the flood itself slowed the PHY thread down).

## Protocol v3 — 2026-10-25 (platform 0.6.0)

- New `NodeToCore::BackhaulStatus { brew_configured, brew_connected }`, sent with every
  `NodeStatus` (after Hello and every 10 s): whether the station has a `[brew]` section and whether
  Brew is connected (`network_connected` in the stack state). The console shows it as the
  station's Brew state. Read-only: nothing is sent towards Brew.
- The station announces protocol 3. It needs platform core 0.6.0 or newer; the core still
  accepts v2 stations, so stations can be updated one by one.

## Bus types — 2026-10-25 (platform 0.5.0)

- `crates/dispatch-proto/src/bus.rs` follows the platform: new `CoreEvent::NodeLink { rtt_ms }`
  (the core's QUIC round-trip time to the station, shown as the station's ping in the console).
  Bus-only type used between platform services; the station↔core wire protocol is unchanged
  (still v2), so stations need no update.

## Protocol v2 — 2026-10-25

- Every group call is monitored for the platform (`MonitorSet { all }`), including network
  speakers: CMCE reports Brew/SIP speakers to the link, UMAC copies network downlink voice to it.
  Receive only — nothing is sent towards Brew.
- Radio commands from the console: kick (CMCE `KickMs`), re-register (MM D-LOCATION UPDATE COMMAND
  with group report, new control command `ReRegister`), position request (LIP IMMEDIATE LOCATION
  REPORT REQUEST as SDS type 4, TS 100 392-18-1 §6.2.16).
- Network-wide ban list in `Settings`: kept in the stack state (`issi_banlist`), checked by MM on
  registration, group attach and reactive recovery; registered banned radios are kicked.
- `NodeStatus` every 10 s (uptime, carriers, MCC/MNC/LA, registered radios, calls, timeslots, CPU
  temperature); `SettingsApplied.banned`.
- Full state (radios, groups, last positions) after every (re)connect and on `ResyncState`.

## Always-on recovery — 2026-10-24

- A station with `[dispatcher]` always runs restart recovery (`recovery_cache.json`).
- Recovery COMMANDs are retried about once a second (one multiframe) instead of every frame, so a
  radio busy on a traffic channel is still reached after its call; an unknown radio seen on the
  uplink is retried until it re-registers.
- Dispatcher calls go on air even when the station knows no listener on the group, and are not
  dropped when the last known listener leaves.
- A pre-emptive (12–15) dispatcher call releases a lower-priority call on the group and sets up its
  own instead of continuing it as a speaker change (EN 300 392-2 §14.8).
- A new PTT while the dispatcher's own previous transmission plays out its buffer replaces it
  instead of being refused as `FloorBusy`.

## Protocol v1 — 2026-10-24

- DGNA and text SDS from the console through the CMCE control channel; positions, RSSI, DGNA status
  and received SDS reported to the platform.

## Stage 0 — 2026-10-23

- QUIC/TLS link to the platform core, dispatcher group calls with priority, monitoring, Brew guard
  in three layers, playout before hangtime.
