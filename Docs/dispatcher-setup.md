# Dispatcher link: setup

**English** · [Українська](uk/dispatcher-setup.md)

How to connect a FlowStation built from this fork to a
[flowstation-platform](https://github.com/hbohatix/flowstation-platform) dispatcher console.
What the link does and how it behaves: [dispatcher-link.md](dispatcher-link.md).

Pick your case:

| Case | Follow |
|---|---|
| Platform and a **test** station on the station's own Raspberry Pi (production station untouched) | flowstation-platform [`deploy/pi/README.md`](https://github.com/hbohatix/flowstation-platform/blob/main/deploy/pi/README.md) — it builds this fork and writes the config for you |
| Station connects to a platform **elsewhere** (PC/server with Docker, another Pi) | this page |

The platform must already run and you must be able to log in to its console
(flowstation-platform [`docs/getting-started.md`](https://github.com/hbohatix/flowstation-platform/blob/main/docs/getting-started.md), steps 1–3).

## 1. Build the station

On the Raspberry Pi of the station (Rust 1.88+, `libsoapysdr-dev`, `pkg-config`, `cmake`):

```bash
git clone -b feature/dispatcher-link https://github.com/hbohatix/flowstation.git
cd flowstation
cargo build --release --bin bluestation-bs
```

Takes about 5 minutes on a Pi 4 after the first download of crates. The binary is
`target/release/bluestation-bs`. With the SIP/Asterisk bridge, build with `--features asterisk`
as in the main README; the dispatcher link is in both builds.

When a station is already running from another build, keep its binary so you can go back:

```bash
cp /path/to/running/bluestation-bs /path/to/running/bluestation-bs.prev
```

## 2. Register the station in the console

In the console, *Base stations → Add a station*: a *Station ID* (e.g. `station-01`), a name,
*Register*. The console shows the **token once**, a ready **`[dispatcher]` snippet** and a link to
the **core certificate** `core-cert.pem`.

Copy the certificate to the station:

```bash
sudo mkdir -p /etc/flowstation
sudo cp core-cert.pem /etc/flowstation/core-cert.pem
```

## 3. Configure

Paste the snippet at the end of the station's `config.toml` and check it:

```toml
[dispatcher]
enabled = true
server = "192.168.1.10:4433"                 # the core, as this station reaches it (UDP)
server_name = "localhost"                    # a name in the core certificate
ca_cert = "/etc/flowstation/core-cert.pem"   # the downloaded certificate
node_id = "station-01"                       # as registered
token = "…"                                  # from the console, shown once
gssis = [10]                                 # dispatch groups — must be local-only
fallback_issi = 9000001                      # caller ISSI until the console sets one
# local_groups = true                        # default: every local-only group is allowed too
# default_priority = 0                       # EN 300 392-2 table 14.46
# max_tx_seconds = 60
# link_timeout_ms = 6000
```

Then the one rule that matters: **dispatch groups are local-only.** Every group in `gssis` must be
inside `local_ssi_ranges` in `[cell_info]`:

```toml
[cell_info]
local_ssi_ranges = [[0, 90]]     # TG 0–90 stay in this cell; 91+ can go to Brew
```

If a dispatch group could be routed to Brew, the station still starts but the dispatcher link does
not, and the log says which group conflicts. Dispatcher calls never reach Brew, whatever the
platform asks (three separate checks, see [dispatcher-link.md](dispatcher-link.md)).

`config.toml` now holds the station's token: keep it out of git (`/config.toml` is in
`.gitignore`) and do not post it.

## 4. Start and check

Restart the station the way you run it, for example:

```bash
sudo systemctl restart bluestation-bs
sudo journalctl -u bluestation-bs -f -o cat
```

Expected in the log:

```
 -> Dispatcher link enabled (core 192.168.1.10:4433)
Dispatcher: link to core is up
Dispatcher: sending state of 3 registered radio(s)
```

In the console: *Stations* shows the station **ONLINE** (uptime, carriers, Brew state, repeater
ID); *Radios* lists its registered radios; *Groups* the groups they listen to.

Next: set the dispatcher ISSI and program the radios (flowstation-platform `docs/getting-started.md`,
steps 3 and 5).

## 5. Change, switch off, go back

| Task | How |
|---|---|
| new token (old one leaked, station replaced) | console *Base stations → New token*, put it into `token`, restart |
| core moved | new `server`; after new core certificates also a new `core-cert.pem` |
| switch the link off | `enabled = false` or remove the section, restart — the station behaves exactly as without the fork's link |
| back to the previous binary | stop the station, put `bluestation-bs.prev` back, start |

## 6. Troubleshooting

| Log line | Meaning, fix |
|---|---|
| `Dispatcher link NOT started: gssis [..] are Brew-routable` | a dispatch group is outside `local_ssi_ranges`; add it there or remove it from `gssis` |
| `Dispatcher link NOT started: … cannot read ca_cert …` | the certificate file is missing or not readable by the station user — check the path and permissions |
| `Dispatcher: cannot reach core (…); retrying in …` | `server` wrong, UDP 4433 blocked on the way, platform down; certificate or `server_name` mismatch also ends here — check the reason in brackets |
| `core refused this node: …` | wrong `node_id` or `token`, token replaced, station disabled or removed in the console |
| `Dispatcher: link to core lost: …` | network or platform restart; the station reconnects by itself with backoff, a dispatcher transmission stops within `link_timeout_ms` |
| `Dispatcher: settings not fully applied: …` | the console sent something the station refused (e.g. a dispatcher ISSI that a radio here already uses); it keeps the previous value |
| `GSSI … became Brew-routable at runtime; refusing the dispatcher call` | the config was edited live and a dispatch group is no longer local |

Change history of the link: [dispatcher-changelog.md](dispatcher-changelog.md).
