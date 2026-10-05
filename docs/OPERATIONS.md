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
