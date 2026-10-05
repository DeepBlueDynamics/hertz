# Hertz Operations Guide

This guide covers daily operations, troubleshooting, data management, and customizing configurations for the Hertz SDR receiver stack.

---

## 1. Running the Daemon

```powershell
cargo build --release -p hertz-daemon
.\target\release\hertzd.exe data\hertz.windows.toml --listen
```

(`./target/release/hertzd hertz.toml --listen` on Linux and macOS.)

- `--listen` plays squelch-gated audio on the default output device and logs one
  console line per squelch open/close and per transcript. If the output device
  disappears (unplugged, monitor asleep), the speaker reopens on the current
  default device every 2 s.
- `--mock` runs with a synthetic dongle to exercise the network surface.
- Stop with Ctrl-C. Windows keeps a running `hertzd.exe` locked, so stop it before
  `cargo build` or `cargo clean`.

The `hertz` client talks to a running daemon (default `http://localhost:9080`,
`--connect` to change, `HERTZ_TOKEN` for auth):

| Command | Does |
| :--- | :--- |
| `hertz` / `hertz tui` | Interactive TUI (`?` for help) |
| `hertz status` | Dongles, frequencies, squelch, recording state |
| `hertz doctor` | USB and device diagnostics (section 4) |
| `hertz records` | List recordings |
| `hertz tail` | Stream live events |
| `hertz tune <freq-or-channel> [--dongle id]` | Retune a dongle |

---

## 2. Directory Layout & Data Management

Everything a running daemon writes goes under `data_dir` (`./data` in the example
configs):

- `recordings/` — one WAV per transmission,
  `transmission_<utc>_Ch<id>_<label>_<freq>MHz_<n>.wav` (48 kHz mono 16-bit).
- `transcriptions/` — one `.txt` per recording, same name, with a header (file,
  channel, frequency, duration, engine, time) and the text, or
  `(no speech detected)`.
- `logs/events.log` — one line per saved recording.
- `history/activity.jsonl`, `history/transcripts.jsonl` — the activity and
  transcript history served by `/api/activity` and `/api/transcriptions`.

All of it is safe to delete while the daemon is stopped; the folders are
recreated on start. Recordings without a transcript are transcribed at startup.

The Whistle model is not stored here: it lives in the Whistle cache
(`%LOCALAPPDATA%\whistle` or `~/.cache/whistle`, about 18 MB). See
[INSTALL.md](INSTALL.md#5-transcription-engine-cactus-whistle).

---

## 3. Bandplans

Bandplans are TOML channel databases. The daemon loads every `*.toml` from the first
of these that exists: `HERTZ_BANDPLANS`, `./bandplans`, `../bandplans`,
`/etc/hertz/bandplans`.

To add channels, add a file to that directory and restart the daemon:

```toml
[[channel]]
id = "custom-ch-01"
name = "Harbor Channel A"
freq_hz = 156_425_000
mode = "nfm"
bandwidth_hz = 20_000
group = "custom-harbor"
label = "HARBOR-A"
```

A separate user-bandplan directory under `data/` is not supported yet.

---

## 4. Diagnostics with `hertz doctor`

`hertz doctor` asks a running daemon for a diagnostic report, for first runs and
troubleshooting:

```bash
hertz doctor
```

It checks:

1. **USB Enumeration**:
   - Checks if any RTL-SDR USB dongles are visible on the USB bus.
   - *Fix if 0 found*: Ensure the dongles are plugged into a powered hub and the USB driver is set up ([INSTALL.md](INSTALL.md)).
2. **Device Claims & Permissions**:
   - Probes device access. If it fails with access errors:
   - *Fix*: Check that the udev rule is installed and reloaded, and that no other process (the kernel DVB module, other SDR software) is holding the device.
3. **PPM/DC Offset Average Sanity**:
   - Performs a 0.5-second test read on each device. Computes the average DC offset for `I` and `Q` channels.
   - An offset close to `0.0` is normal. If offsets are extremely large (e.g., `>10.0`), it warns of potential hardware issues or excessive local interference.
4. **Serial Uniqueness**:
   - Ensures each connected dongle has a unique serial number in its EEPROM.
   - *Fix*: If duplicates are found, use `rtl_eeprom -s` to assign unique serials (e.g., `MARINE01`, `PUBLIC01`), as duplicate serials cause role assignment conflicts in the config.

---

## 5. Relay to Hyperia Panes

With a `[relay]` section, every transcript is routed to the Hyperia pane it names.
Say the pane's name on the air ("Top Rabbit, come in, over"):

1. hertzd pulls the live pane list from Hyperia (`terminal_status`).
2. ollaya ([OLLAYA.md](OLLAYA.md)) picks which pane the transcript addresses, with
   an explicit "none of these panes" option. There is no fuzzy matching: a pick
   below `min_probability` (default 0.5), or "none", is not routed.
3. A match is sent to that pane with Hyperia's `msg_send` (subject
   `[radio] Ch 71 156.575 MHz`, body = the transcript). The pane gets a
   `[Hyperia mail]` notice. Hyperia asks you once per pane whether hertz may
   message it; after that it delivers unattended.

The console shows the outcome of every call:

```
RELAY | Ch 72 | -> Top Rabbit 🧟 (p=0.98) | awaiting_approval
RELAY | Ch 72 | double-check heard more (p=0.63): "Top Rabbit, Top Rabbit, this is Alpha Tango, come in, over." -> follow-up to Top Rabbit 🧟 | ...
RELAY | Ch 71 | can't find a pane for "Alpha India, come in" (best guess "(none of these panes)", p=0.55)
```

`awaiting_approval` is Hyperia holding the first message to a pane until you
approve; later messages to that pane go straight through.

**Double-check.** After a call is sent, if the transcription service at
`verify_url` (default `http://localhost:8765`, Whisper `verify_model` = `medium`)
is up, hertzd re-transcribes the recording there. ollaya is given the words each
transcript heard that the other didn't and decides whether the server version
adds real information (a call sign, name, place, number or instruction) or only
differs trivially. If it adds something, a follow-up goes to the same pane with
subject `Re: [radio] Ch 72 156.625 MHz`, the server transcript and the original.
The check never delays the first message, is skipped while the service is down,
and is turned off with `verify_url = ""`.

Only live transmissions are relayed; transcripts made for old recordings at
startup are not.

### One-time setup: the `hertz-radio` identity

hertzd talks to Hyperia as its own agent identity, not a pane's (pane tokens die
with the pane). Register it once, without a pane token so the mailbox isn't bound
to your pane, and store the token outside the repo:

```powershell
$r = Invoke-RestMethod http://localhost:9800/mcp -Method Post -ContentType application/json `
  -Headers @{ Accept = 'application/json, text/event-stream' } `
  -Body '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_token","arguments":{"name":"hertz-radio","single_session":true}}}'
# Save the hyp_agent_... token from the reply to ~/.hertz/hyperia-token (user-only permissions).
```

Check which pane a call would reach, without sending anything:

```bash
cargo run -p hertz-relay --example route -- "Top Rabbit, come in, over."
cargo run -p hertz-relay --example verify -- data/recordings/<file>.wav "<on-device text>"
```
