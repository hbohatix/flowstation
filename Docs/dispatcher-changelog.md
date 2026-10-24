# Dispatcher link — changelog

**English** · [Українська](uk/dispatcher-changelog.md)

Changes of the `[dispatcher]` link in this fork (branch `feature/dispatcher-link`). The platform
side: [flowstation-platform CHANGELOG](https://github.com/hbohatix/flowstation-platform/blob/main/CHANGELOG.md).

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
