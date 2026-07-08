# Hertz — Build Plan

A ground-up rewrite of gnosis-radio as a containerized, multi-dongle, TUI-only radio stack
for boats: always-on scanning and monitoring of marine VHF and the public radio bands, an
MCP server throughout with subscribable streaming endpoints, pluggable transcription
(in-container Whisper or external/cloud service), and a policy-gated transmit path so an AI
can key a real transmitting radio on channels where that is legal.

Companion documents:
- `plan/breif.md.txt` — the research brief (DSP architecture, crate ecosystem, squelch design).
- `plan/gnosis-feature-inventory.md` — the full feature parity checklist extracted from
  gnosis-radio. Everything in that file is in scope unless marked "drop".

---

## 1. Mission and constraints

**Deployment picture:** a boat. The captain has a real marine VHF transceiver for talking.
Hertz runs headless on an onboard Linux box (mini-PC / Pi 5) with one or more RTL-SDR
dongles on a powered USB hub, inside a Docker container that owns the USB bus. The TUI is
launched from an external docker command and attaches to the daemon. An AI agent (Claude,
via MCP) can see everything the radio hears — activity, transcripts, spectra — and, if a
TX-capable certified radio is attached, transmit on channels where policy allows.

**Hard constraints:**
1. **TUI only.** No web UI. The gnosis-radio browser app is dropped; ratatui is the one face.
2. **Multi-dongle.** N dongles, each assigned a role (channelized band capture, hop-scan
   list, or fixed monitor) from config, selected by serial number, not index.
3. **Container owns the USB bus.** The daemon runs in Docker with `/dev/bus/usb` passed
   through; the TUI runs from a separate `docker exec` / `docker run` invocation.
4. **RX is always legal; TX is policy-gated.** The SDR never transmits. TX only happens by
   keying an attached FCC-certified radio, and never on marine VHF from this system.
5. **MCP throughout.** Control, state, history, and streaming subscriptions all exposed to
   agents; network-exposable with token auth.
6. **Transcription is pluggable.** whisper.cpp in the container, the existing external
   Whisper HTTP service, or a cloud API — chosen in config, swappable at runtime.

---

## 2. Architecture

```
                        ┌────────────────────── docker container: hertzd ──────────────────────┐
 USB bus                │                                                                       │
 ┌─────────┐  libusb    │  ┌───────────┐   IQ    ┌──────────────────┐  audio/events             │
 │ RTL-SDR ├────────────┼─▶│ SdrWorker │────────▶│ DSP Pipelines    │───────┐                   │
 │  #A     │            │  │ (thread   │  ring   │ (channelizer,    │       ▼                   │
 └─────────┘            │  │  per      │  buffer │  NFM/AM demod,   │  ┌──────────┐             │
 ┌─────────┐            │  │  dongle)  │         │  entropy squelch,│  │ EventBus │◀── control  │
 │ RTL-SDR ├────────────┼─▶│           │         │  AFC, prebuffer) │  │ (tokio   │    plane    │
 │  #B     │            │  └───────────┘         └──────────────────┘  │ broadcast│             │
 └─────────┘            │        ▲                        │            └────┬─────┘             │
 ┌─────────┐   serial + │  ┌─────┴─────┐          ┌───────▼────────┐        │                   │
 │ GMRS/ham│   USB audio│  │ TxWorker  │◀─policy──│ Recorder       │        ├─▶ WS /stream      │
 │ radio   ├────────────┼─▶│ (PTT+CAT+ │   engine │ (WAV/txn +     │        ├─▶ SSE /events     │
 │ (option)│            │  │  TTS)     │          │  transcription │        ├─▶ HTTP REST       │
 └─────────┘            │  └───────────┘          │  queue)        │        ├─▶ HTTP /audio PCM │
                        │                         └────────────────┘        └─▶ MCP  /mcp       │
                        └───────────────────────────────────────────────────────────────────────┘
                                              ▲ port 9080 (one port for everything)
        ┌─────────────────────────────────────┴───────────────────────────┐
        │ hertz tui  (docker exec -it hertzd hertz tui                    │
        │             or docker run --rm -it hertz tui --connect ...)     │
        │ MCP agents (Claude etc.), external subscribers                  │
        └─────────────────────────────────────────────────────────────────┘
```

**Two binaries, one protocol.** `hertzd` (daemon) owns hardware, DSP, storage, transcription,
policy, and all servers. `hertz` (client CLI) hosts the TUI plus small subcommands
(`status`, `doctor`, `tune`, `records`). The TUI is a pure client — it renders what the
daemon streams and sends control requests. This is what makes "TUI from an external docker
command" work: the TUI process needs no USB, no DSP, just a socket. It also means the TUI
can run on the laptop at the helm against the daemon in the engine-room box.

**Threading model (per brief + gnosis lessons):** one blocking USB reader thread per dongle
pushing IQ into a lock-free ring; one DSP thread per dongle consuming it (running N per-channel
pipelines inline, as gnosis-radio proved viable at 2.4 MS/s); control flows back via atomics/
watch channels. The async world (axum, MCP, WS, transcription queue) lives on tokio and talks
to the DSP threads only through the EventBus and command channels — no locks shared with the
hot path.

### Workspace layout

```
hertz/
  Cargo.toml                 # workspace
  crates/
    hertz-types/             # shared serde types: events, channels, config, API DTOs
    hertz-sdr/               # device layer: enumeration by serial, rtl-sdr driver, ring buffers
    hertz-dsp/               # ported pipeline: channelizer, demods, squelch, AFC, entropy pool
    hertz-channels/          # channel database loader + bundled bandplan data (TOML)
    hertz-daemon/            # hertzd binary: workers, event bus, recorder, transcription,
                             #   REST/WS/SSE servers, MCP server, TX policy engine
    hertz-tx/                # transmit subsystem (feature-gated): PTT/CAT drivers, TTS glue
    hertz-tui/               # hertz binary: ratatui client + CLI subcommands
  bandplans/                 # channels-*.toml data files (~400 channels, see §4)
  docker/                    # Dockerfile, compose.yaml, udev rules, install scripts
  docs/                      # INSTALL.md (drivers), OPERATIONS.md, LEGAL.md
  plan/                      # this plan
```

### Key crate choices

| Concern | Choice | Rationale |
|---|---|---|
| RTL-SDR driver | `librtlsdr-rs` (pure Rust, rusb) behind a `SdrDevice` trait | No C lib in the container, V4 + all five tuner families, async streaming; the trait keeps a SoapySDR door open for other frontends. Fallback if it disappoints: `rtl-sdr-rs` (port of the Blog fork). |
| DSP | `rustfft`, `num-complex`, hand-rolled FIR/DDC ported from gnosis | The gnosis pipeline is proven on-air; the brief confirms roll-your-own is right for a fixed pipeline. |
| Async/server | `tokio` + `axum` (REST, WS upgrade, SSE) on **one port, 9080** | gnosis's three ad-hoc servers (tiny_http, tungstenite, raw TCP) collapse into one; one `-p` flag in Docker. |
| MCP | `rmcp` (official Rust MCP SDK), Streamable HTTP transport mounted at `/mcp` | Network-exposable, sessions, notifications for subscriptions. |
| TUI | `ratatui` + `crossterm`, `tokio-tungstenite` client | Finishes what gnosis's stub started. |
| Audio out (client) | `cpal` in the TUI + `/audio` PCM endpoint for anything else (VLC) | Speakers belong to whoever runs the TUI, not the container. Optional ALSA passthrough for in-container playback. |
| Recording | `hound` | Same as gnosis. |
| Transcription | trait `Transcriber` with three impls (see §6) | whisper-rs (in-container), external HTTP, cloud. |
| TTS (TX path) | `piper-rs` in-container, or cloud TTS | Feature-gated with hertz-tx. |

---

## 3. Multi-dongle model

Config assigns each physical dongle (by EEPROM serial — `rtl_eeprom -s` lets us burn unique
serials, and `hertz doctor` walks the user through it) a **role**:

```toml
# /etc/hertz/hertz.toml
[daemon]
listen = "0.0.0.0:9080"
data_dir = "/data"                    # recordings/, transcripts/, extracts/, logs/
auth_token = "env:HERTZ_TOKEN"        # required for non-localhost + all MCP writes

[[dongle]]
serial = "MARINE01"
role = "channelized"                  # one wideband capture, all channels demodulated at once
bandplan = "marine-vhf-us"            # 2.4 MS/s @ 156.7375 MHz — whole band, zero retuning
tap_channel = "16"                    # always-on audio tap (gnosis feature, kept)
squelch_db = 12.0
record = true

[[dongle]]
serial = "PUBLIC01"
role = "hopscan"                      # analog-scanner style retune-and-dwell
groups = ["frs-gmrs", "murs", "noaa-wx", "ham-2m-simplex", "railroad-aar"]
dwell_ms = 150                        # per-stop power check; lock while squelch open
priority = ["noaa-wx:WX2"]            # interleaved priority checks during lock
squelch_db = 9.0
record = true

# a third dongle could be role = "monitor" pinned to one frequency, gnosis-monitor style
```

**Scan strategies** (straight from the brief):
- `channelized` — tune once, FFT-detect across every channel in the capture, spawn a DDC +
  pipeline per active channel (gnosis wideband mode, generalized beyond marine). Works for
  any group whose span ≤ 2.4 MHz: marine VHF, CB (440 kHz), FRS/GMRS 462 block, MURS,
  NOAA (150 kHz), GMRS 467 inputs.
- `hopscan` — retune through a channel list, ~10–20 ch/s ceiling, dwell while squelch open,
  hang, resume. State machine: `Scanning → Locked → Hang → Scanning` with priority-channel
  interleave. For lists that span more than 2.4 MHz (mixing 2 m with UHF GMRS, airband…).
- `monitor` — fixed single channel at 240 kS/s (gnosis monitor mode).

Each dongle's worker is independent: its own USB reader thread, ring, DSP thread, noise
floors, and pipelines. All emit into the shared EventBus tagged with `dongle_id`. A dongle
unplugged mid-run emits `DongleLost` and the worker retries enumeration; the daemon never
crashes because one radio fell off the hub (boats vibrate).

---

## 4. Channel database (~400 channels, data not code)

gnosis hardcoded 48 marine channels in Rust. Hertz ships bandplans as TOML data in
`bandplans/`, loaded at startup, user-extensible via the data dir. Each entry:

```toml
[[channel]]
id = "gmrs-17"
name = "GMRS 17"
freq_hz = 462_600_000
mode = "nfm"                # nfm | am | usb | lsb | wfm(future)
bandwidth_hz = 20_000
group = "frs-gmrs"
label = "GMRS-MAIN"
rx = true
tx_policy = "certified-radio+gmrs-license"   # see §7 policy vocabulary
ctcss_hz = 0.0              # optional tone gate (Goertzel, brief §squelch)
notes = "Shared FRS/GMRS; repeater output"
```

Bundled groups (counts approximate; total lands at **~430 channels** — the "400 some odd"):

| Group | Channels | Range / mode | TX policy class |
|---|---|---|---|
| `marine-vhf-us` (incl. WX) | 55 | 156–162 MHz NFM | **never** (type-accepted marine radios only) |
| `marine-vhf-intl` extras | 30 | intl duplex variants | never |
| `marine-ais` | 2 | 161.975 / 162.025 (data) | never |
| `noaa-wx` | 7 | 162.400–162.550 NFM, continuous carrier → manual squelch flag | never (RX only by nature) |
| `frs-gmrs` | 30 | 462/467 MHz NFM (22 shared + 8 GMRS repeater inputs) | certified radio; FRS license-free, GMRS needs the ~$35 no-test 10-yr FCC license |
| `murs` | 5 | 151.820–154.600 NFM | certified radio; license-free |
| `cb` | 40 | 26.965–27.405 AM/SSB | certified radio; license-free (V4 dongle reaches it via built-in upconverter path) |
| `ham-2m` | 45 | 144–148 simplex + common repeater pairs, NFM | SDR-or-radio; ham license + callsign |
| `ham-70cm` | 25 | 430–450 simplex/repeaters | ham license |
| `airband` | 30 | 118–137 AM (121.5 guard, local tower/ground/ATIS/CTAF) | **never** |
| `railroad-aar` | 97 | 160.215–161.565 NFM | never |
| `public-safety-interop` | 20 | VCALL/VTAC + marine mutual aid | never |
| `weather-fax/misc` | ~10 | HF slices, time stations (future-mode stubs) | never |

Channelized-capture centers per group are precomputed in the bandplan file (e.g. `cb` →
27.185 MHz @ 480 kS/s). NOAA channels carry `continuous_carrier = true` so the adaptive
squelch skips floor-tracking on them (the brief's ATIS caveat).

**Demod modes:** NFM ports from gnosis; **AM** (envelope, DC-block, AGC) is new and required
for CB + airband; **SSB** (filter method: ±1.5 kHz shift, 300–2700 Hz complex bandpass) for
upper CB — implemented in that order per the brief's build sequence.

---

## 5. The daemon: event bus and APIs

### EventBus
`tokio::sync::broadcast` fan-out of a single tagged event enum (superset of gnosis's
`AudioMessage`, everything additionally carries `dongle_id`):

`Audio` (48 kHz f32 frames) · `SignalLevel` · `SquelchEvent` · `ChannelActivity` (heartbeat,
even when idle — gnosis learned UIs look dead otherwise) · `Transcription` · `Translation` ·
`RecordingSaved` · `ScanState` (hopscan position/lock) · `DongleStatus` · `TxEvent`
(request/approved/keyed/unkeyed/blocked) · `VoicePaint`.

History accumulator (gnosis `AgenticState`, generalized): ring buffers of last 500 activity
events, 200 transcripts, 50 TX events; persisted to `data_dir` as JSONL so restarts don't
amnesia the day's log.

### One port, four surfaces (axum on 9080)

1. **REST** — parity with gnosis plus multi-dongle addressing:
   `GET /api/status`, `GET /api/dongles`, `POST /api/dongles/{id}/tune`,
   `POST /api/dongles/{id}/squelch`, `POST /api/dongles/{id}/recording`,
   `GET /api/channels?group=`, `GET /api/activity`, `GET /api/transcriptions`,
   `GET /api/recordings[/{file}]`, `GET|POST /api/extracts`, `GET /api/entropy`,
   `GET /api/time`, `POST /api/clean-transcript`, `POST /api/tx/request` (§7).
2. **WS `/stream`** — the subscribable streaming endpoint: JSON events + binary audio frames
   (gnosis wire format kept: `[u32 ch][u32 freq][f32 dB][f32 PCM…]`, prefixed with a dongle
   byte). Query params filter: `?events=transcription,squelch&audio=marine-vhf-us:16`.
3. **SSE `/events`** — JSON-only event stream for curl-grade subscribers; `/api/entropy/stream`
   rides here too.
4. **HTTP `/audio`** — chunked raw PCM L16 (gnosis-compatible; VLC-playable at the nav desk).

Auth: `Authorization: Bearer` token on everything non-localhost; TUI reads it from env/flag.

### MCP server (`/mcp`, Streamable HTTP via rmcp)

**Tools:** `radio_status` · `list_channels` · `list_dongles` · `tune` · `set_squelch` ·
`set_recording` · `scan_control` (pause/resume/lock/skip) · `get_activity` ·
`get_transcripts(since, group)` · `get_recording(file)` (base64/URL) · `analyze_spectrogram`
(voicepaint, daemon-rendered) · `drain_entropy` · `transmit_voice(channel, text, confirm)` —
policy-gated, see §7 · `transmit_status`.

**Resources:** `hertz://channels/{group}`, `hertz://transcripts/recent`,
`hertz://activity/recent`, `hertz://config` — with `resources/subscribe` so an MCP client
gets `notifications/resources/updated` pushes when new transcripts land. For firehose
consumption agents are pointed at WS `/stream` (the MCP tool `radio_status` returns the URL
+ token); MCP handles command/control and change-notification, WS handles bulk streaming.
This is the "MCP server throughout, exposed, subscribe to a streaming endpoint" requirement.

---

## 6. Transcription and translation (pluggable)

```toml
[transcription]
engine = "whisper-internal"    # whisper-internal | whisper-http | cloud | off
model = "small"                # internal: tiny|base|small|medium (ggml, fetched at build or first run)
language = "auto"
translate_to = ""              # e.g. "en" — adds a Translation event via the same engine or Claude

[transcription.whisper_http]  # the existing gnosis service, unchanged wire protocol
url = "http://host.docker.internal:8765"
model = "large-v3"

[transcription.cleanup]
enabled = true                 # Claude Haiku marine-shorthand pass (gnosis prompt carried over)
```

- `whisper-internal`: whisper-rs (whisper.cpp bindings), CPU, runs in-container; `small`
  is the default balance for marine radio audio on a mini-PC. Queue with one worker so DSP
  never blocks; transcripts append to `transcriptions/*.txt` beside recordings, exactly like
  gnosis's layout.
- `whisper-http`: gnosis's multipart submit/poll/download client, ported as-is.
- `cloud`: OpenAI-compatible audio endpoint or Anthropic — one HTTP impl, base URL + key in config.
- Translation: optional second pass (Whisper translate-to-English natively, or Claude for
  other target languages); emitted as a distinct `Translation` event so the TUI can show both.

---

## 7. Transmit subsystem (feature `tx`, off by default)

**Legal reality baked into the design** (encoded per-channel in the bandplan, enforced by a
policy engine, not by prompts):

| Path | Legality | Hertz behavior |
|---|---|---|
| Marine VHF via anything we control | Requires type-accepted marine radio + operational rules | **Hard-blocked.** `tx_policy = "never"`; not overridable by config. The captain uses his own VHF. |
| FRS / GMRS / MURS / CB via SDR (HackRF etc.) | Illegal — Part 95 requires FCC-certified transmitters | **Hard-blocked.** No SDR TX path exists in v1 at all. |
| FRS/GMRS/MURS/CB via attached **certified radio** (PTT + audio) | FRS/MURS/CB license-free; GMRS needs the ~$35, 10-year, no-test FCC license (covers family) | Allowed when a TX radio is configured, the channel's policy class matches, and for GMRS `license.gmrs = "WXXX000"` is set in config. |
| Ham bands (2 m / 70 cm) | Licensed operators may use any transmitter incl. SDR; station ID every 10 min; control-operator rules for automatic operation | Allowed with `license.ham_callsign` set; auto-CW/voice ID timer; (SDR TX deferred to v2 — v1 still keys a radio, e.g. a CAT-controlled transceiver) |
| Airband, railroad, public safety | No | Hard-blocked. |

**Hardware path (v1):** an FCC-certified radio (Baofeng-class GMRS/ham HT or a mobile with
CAT) connected via (a) USB sound card to mic/speaker jacks, (b) PTT via serial RTS/DTR or
CM108 GPIO (the ham-standard AllStar/APRS cabling), and optionally (c) CAT serial for
frequency control where supported. All passed into the container as additional `devices:`.

**Policy engine** (in-daemon, every TX request flows through it, MCP or TUI alike):
channel `tx_policy` class must be satisfiable by configured licenses + attached hardware →
duty limits (max key-down 60 s watchdog, min 2 s between transmissions, per-hour cap) →
optional `require_human_confirm = true` (TUI modal / MCP returns `pending` until confirmed)
→ ID timer for ham → immutable TX log (JSONL + WAV of what was sent). Every decision emits
a `TxEvent`, so the AI transmitting is always visible in the TUI.

**Voice:** `transmit_voice(channel, text)` → TTS (piper in-container; cloud optional) →
resample → key PTT → play → unkey → log + broadcast. Also `transmit_wav` for canned calls.

**Scan-only fallback:** with no TX radio configured (`[tx]` absent), the subsystem
compiles out / stays dormant and Hertz is a pure monitor — the default posture.

---

## 8. TUI (`hertz tui`)

ratatui client speaking WS to the daemon. Layout (function keys switch panes, vim keys move):

```
┌ Dongles ──────────────┬ Band Scope (waterfall of selected dongle) ──────────────┐
│ ▸ MARINE01 channelized│  156.0 ▁▂▁▁▃▁▁▁▁▂█▂▁▁▁▁▂▁▁ 157.5   [FFT bins → color]   │
│   PUBLIC01 hopscan    │                                                          │
├ Channel Grid ─────────┴──────────────────────────────────────────────────────────┤
│ CH16 SAFETY ▂▂  CH22 USCG ▁   GMRS17 ▆ ACTIVE  WX2 ▃  MURS3 ▁  146.520 ▁ ...    │
├ Activity ────────────────────────────┬ Transcript ──────────────────────────────┤
│ 18:42:07 ▲ GMRS17 −12 dB VOICE       │ 18:42:15 GMRS17: "…heading back to the   │
│ 18:42:15 ▼ GMRS17 saved #14          │  marina, see you at the dock"            │
│ 18:40:51 ▲ CH16 −9 dB VOICE          │ 18:40:58 CH16: "SECURITE …"  [translate] │
├ Status bar ──────────────────────────┴──────────────────────────────────────────┤
│ ● REC  ♪ LISTEN GMRS17  SQL 12dB  TX:disabled  MCP:2 clients  9080 ok  UTC 18:42│
└──────────────────────────────────────────────────────────────────────────────────┘
```

- Panes: dongle list, spectrum/waterfall (from `SignalLevel` + periodic FFT snapshot events),
  channel activity grid (per-group pages), scrolling event log, transcript pane (with
  translation lines), recordings browser (play via local cpal), TX console (hidden unless
  TX enabled: compose → confirm → watch `TxEvent`s), config/squelch dialog.
- Keys: `space` listen-toggle on selected channel, `r` record toggle, `s` squelch dialog,
  `t` tune/frequency entry, `↑↓` channel select, `tab` dongle, `x` TX console, `q` quit.
- Audio: cpal playback in the TUI process of whichever channel is "listened" (volume-scaled
  0.035 default, as gnosis). `--no-audio` for SSH sessions; `/audio` endpoint covers those.
- Also plain CLI subcommands for scripting: `hertz status|doctor|tune|records|tail`.

The ASCII viz renderers from gnosis (`viz.rs`) become the waterfall/waveform widgets —
ported, not rewritten.

---

## 9. Container and USB

### Dockerfile (multi-stage)
- `rust:bookworm` builder → `debian:bookworm-slim` runtime. Pure-Rust driver means no
  librtlsdr apt package needed; add `ca-certificates`, optional whisper ggml model layer
  (or fetch to `/data/models` at first run to keep the image slim).
- Both binaries in the image; `ENTRYPOINT ["hertzd"]`, TUI reachable via
  `docker exec -it hertzd hertz tui`.

### compose.yaml
```yaml
services:
  hertzd:
    image: ghcr.io/deepbluedynamics/hertz:latest
    container_name: hertzd
    restart: unless-stopped
    devices:
      - /dev/bus/usb:/dev/bus/usb          # the USB bus, per requirement
    device_cgroup_rules:
      - 'c 189:* rmw'                      # survive replug/renumeration (hotplug)
    environment:
      - HERTZ_TOKEN=${HERTZ_TOKEN}
    ports:
      - "9080:9080"
    volumes:
      - ./data:/data
      - ./hertz.toml:/etc/hertz/hertz.toml:ro
      # TX option: add the radio's serial + sound devices, e.g.
      # - /dev/ttyUSB0:/dev/ttyUSB0
      # - /dev/snd:/dev/snd
```

TUI invocations (both documented in README):
```bash
docker exec -it hertzd hertz tui                       # on the box
docker run --rm -it ghcr.io/deepbluedynamics/hertz \
  hertz tui --connect http://boat.local:9080 --token $HERTZ_TOKEN   # from anywhere
```

### `hertz doctor`
First-run diagnostic inside the container: enumerates USB, checks dongle access/permissions,
detects the dvb kernel-module conflict, verifies sample flow (reads 1 s of IQ and prints a
mini spectrum), checks EEPROM serials for uniqueness and offers to write them, tests the
transcription engine, and prints exactly which fix from INSTALL.md applies. This turns 90%
of driver-support questions into one command.

### Driver installation (docs/INSTALL.md — content, not just a stub)

**Linux host (the supported boat deployment — Pi 5 / any mini-PC):**
1. Kernel DVB driver must not claim the dongle:
   `echo 'blacklist dvb_usb_rtl28xxu' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf`
   then `sudo modprobe -r dvb_usb_rtl28xxu` (or reboot).
2. udev permissions (repo ships `docker/60-hertz-rtlsdr.rules` covering RTL2832U VID:PIDs
   `0bda:2832`, `0bda:2838`, MODE 0666 TAG uaccess):
   `sudo cp docker/60-hertz-rtlsdr.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger`
3. Plug dongles into a **powered** hub, `docker compose up -d`, `docker exec hertzd hertz doctor`.

**Windows host (dev machines like this one):** Docker Desktop cannot pass USB directly;
use usbipd-win → WSL2:
1. `winget install usbipd`
2. `usbipd list` → find the RTL-SDR (Bulk-In interface) → `usbipd bind --busid <id>`
3. `usbipd attach --wsl --busid <id>` (re-attach after replug; `--auto-attach` for persistence)
4. Inside the WSL2 distro, apply the Linux udev steps, then run compose from WSL2.
Native (non-container) Windows runs remain possible for development: Zadig → install WinUSB
on "Bulk-In, Interface 0" — the classic RTL-SDR Windows driver dance — documented as the
dev-mode fallback.

**macOS host:** Docker Desktop has no USB passthrough. Documented answer: run the daemon
natively (`cargo install`, librtlsdr not required with the pure-Rust driver) or use a Linux
box; the TUI connects remotely either way.

---

## 10. Feature parity map (gnosis → hertz)

| gnosis-radio | hertz |
|---|---|
| monitor mode | dongle `role = "monitor"` |
| wideband scan (marine) | `role = "channelized"`, any ≤2.4 MHz bandplan group |
| — | `role = "hopscan"` (new, brief's hop-scan state machine) |
| entropy squelch, AFC, prebuffer, voice gate, fades | ported verbatim into `hertz-dsp` with unit tests over recorded IQ fixtures |
| always-on tap channel | `tap_channel` per dongle |
| WAV-per-transmission + naming + logs | same format, under `/data`, plus JSONL event log |
| HTTP control API (9080) | REST on 9080 (superset, dongle-addressed) |
| WS audio+events (9081) | WS `/stream` on 9080 (same binary frame + dongle tag) |
| `/audio` PCM chunked | kept |
| TCP `--stream` NDJSON push | kept as optional `[push_stream]` config (freq_monitor compat) |
| external Whisper service | `engine = "whisper-http"` (wire-compatible) |
| — | `engine = "whisper-internal"` (whisper.cpp in-container), `cloud`, translation |
| Claude transcript cleanup | kept (`[transcription.cleanup]`) |
| voicepaint via browser screenshot | daemon renders spectrogram PNG itself → Claude vision → same JSON |
| entropy pool + SSE | kept |
| `/api/time`, extracts API | kept |
| embedded web UI, window layout API | **dropped** (TUI-only mandate) |
| cpal speaker in daemon | moved to TUI client (+`/audio` for others) |
| hardcoded marine channel table | `bandplans/*.toml`, ~430 channels |
| `rtlsdr 0.1` + vendored .lib | pure-Rust driver, Linux container |
| single dongle | N dongles by serial, roles, hotplug recovery |
| — | MCP server, TX policy engine + certified-radio TX path |

---

## 11. Build order (each phase ends runnable and testable)

**Phase 0 — Scaffold (½ day):** workspace, `hertz-types`, config loader, bandplan TOML
schema + the ~430-channel data files, CI (fmt/clippy/test), Dockerfile skeleton.
*Test: `cargo test` loads and validates every bandplan; channel↔freq round-trips.*

**Phase 1 — SDR layer (1–2 days):** `hertz-sdr` — enumerate by serial, open, configure
(rate/freq/gain/bandwidth), reader thread → ring buffer; `hertz doctor` v0.
*Test: `hertzd --dump-fft` prints a live ASCII spectrum from each configured dongle (brief's
step 1). Works against NOAA 162.55 as the always-on beacon.*

**Phase 2 — DSP port (2–3 days):** port gnosis pipeline into `hertz-dsp` as a pure library:
NFM demod, channel FIR, entropy squelch, AFC, prebuffer, recorder, classification; add AM.
Record IQ fixtures (from phase 1) and pin squelch open/close behavior in unit tests.
*Test: single-channel NOAA NFM → WAV out matches gnosis quality; CB AM demod on fixture.*

**Phase 3 — Daemon core (2–3 days):** EventBus, per-dongle workers for `monitor` +
`channelized` roles, recorder + history persistence, REST + WS + SSE + `/audio` on axum,
auth. *Test: two dongles live — marine channelized + NOAA monitor — events visible via
`websocat`, recordings rotate per transmission.*

**Phase 4 — TUI (2–3 days):** ratatui client, all panes except TX console, cpal playback,
CLI subcommands. *Test: `docker exec -it hertzd hertz tui` end-to-end on the boat box;
remote `--connect` from Windows laptop.*

**Phase 5 — Hopscan + full bandplans (1–2 days):** hop-scan state machine with dwell/hang/
priority, CTCSS Goertzel gate, SSB demod for upper CB. *Test: scan frs-gmrs+murs+noaa on
dongle B while dongle A holds marine; lock/resume timing sane; ch 19 CB AM audible.*

**Phase 6 — Transcription (1–2 days):** `Transcriber` trait, whisper-internal +
whisper-http + cloud, cleanup pass, translation events, transcript persistence.
*Test: transmission → transcript in TUI within seconds on `small` model, CPU-only.*

**Phase 7 — MCP (1–2 days):** rmcp Streamable HTTP at `/mcp`, tools + resources +
subscriptions, token auth. *Test: Claude Code connects, tunes a channel, reads transcripts,
gets a resource-updated notification when a new one lands.*

**Phase 8 — TX subsystem (2–3 days, feature `tx`):** policy engine + bandplan policy
classes, PTT (serial RTS + CM108) and USB-audio drivers, TTS, TX console pane, MCP
`transmit_voice`, watchdogs, TX log. *Test: on GMRS with a certified HT + valid license
config: MCP-requested voice TX round-trips and is heard on a handheld; marine VHF TX request
returns `blocked: policy` no matter what config says.*

**Phase 9 — Container polish + docs (1–2 days):** compose hardening, hotplug cgroup rules,
INSTALL.md (drivers, usbipd, udev), OPERATIONS.md, LEGAL.md, `hertz doctor` final,
image publish. *Test: fresh Linux box from zero to TUI in the documented steps only.*

---

## 12. Risks and mitigations

- **`librtlsdr-rs` maturity / multi-dongle quirks** → `SdrDevice` trait isolates it; fallback
  crates identified; phase 1 proves it before anything is built on top.
- **CPU on a Pi-class box** (2 dongles × 2.4 MS/s + whisper) → gnosis already runs the
  channelized path on one core-class budget; whisper `small` quantized on the worker queue;
  worst case `tiny` or external service. Bench in phase 6 on target hardware.
- **USB bandwidth/power on a hub** (two dongles ≈ 2× 4.8 MB/s iso/bulk) → powered hub
  mandated in docs; doctor detects dropped samples and says so.
- **Squelch regressions vs gnosis** (its entropy squelch is field-tuned) → port constants
  verbatim, pin behavior with IQ-fixture tests before touching anything.
- **TX legal exposure** → policy classes live in signed-default bandplans; marine/air/rail
  are `never` at the type level (not config-reachable); TX absent unless feature-built and
  configured; immutable TX audit log.
- **NOAA continuous carrier defeats adaptive floor** → `continuous_carrier` flag, manual
  threshold (the brief called this out; gnosis never handled it).
- **Windows/macOS container USB gaps** → documented honestly (usbipd for Windows, native or
  Linux box for macOS); the boat target is Linux.

## 13. Out of scope for v1 (parked)

SDR transmit (HackRF on ham bands), digital voice (DMR/P25), AIS decoding (channels reserved
in the bandplan), ADS-B, trunked systems, DSC ch 70 decoding (channel exists, decode later),
web UI of any kind, Windows-native daemon packaging.
