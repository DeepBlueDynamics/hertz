# Hertz Operations Guide

This guide covers daily operations, troubleshooting, data management, and customizing configurations for the Hertz SDR receiver stack.

---

## 1. Running the Container Stack

> [!WARNING]
> The image does not build until the `whistle` crate is vendored into this repo; see
> [INSTALL.md](INSTALL.md#step-3-run-the-container-stack).

### Starting the Stack
To start the daemon in the background:
```bash
docker compose -f docker/compose.yaml up -d
```

### Stopping the Stack
To stop the daemon without losing data:
```bash
docker compose -f docker/compose.yaml down
```

### Viewing Logs
To view daemon log outputs in real time:
```bash
docker compose -f docker/compose.yaml logs -f
```

---

## 2. Running Natively (Windows/Linux/macOS)

```powershell
cargo build --release -p hertz-daemon
.\target\release\hertzd.exe data\hertz.windows.toml --listen
```

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
| `hertz doctor` | USB and device diagnostics (section 5) |
| `hertz records` | List recordings |
| `hertz tail` | Stream live events |
| `hertz tune <freq-or-channel> [--dongle id]` | Retune a dongle |

---

## 3. Directory Layout & Data Management

Everything a running daemon writes goes under `data_dir` (`./data` natively,
`/data` in the container, which compose maps to `docker/data` on the host):

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

## 4. Bandplans

Bandplans are TOML channel databases. The daemon loads every `*.toml` from the first
of these that exists: `HERTZ_BANDPLANS`, `./bandplans`, `../bandplans`,
`/etc/hertz/bandplans`. The container sets
`HERTZ_BANDPLANS=/usr/share/hertz/bandplans`.

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

In the container, mount your directory over `/usr/share/hertz/bandplans` (or
point `HERTZ_BANDPLANS` at a mounted path). A separate user-bandplan directory
under `data/` is not supported yet.

---

## 5. Diagnostics with `hertz doctor`

The `hertz doctor` utility is a first-run and troubleshooting diagnostic script designed to identify configuration issues.

To run it:
```bash
docker exec -it hertzd hertz doctor
```

### Understanding Doctor Output Fields
`hertz doctor` outputs a structured diagnostic report checking:

1. **USB Enumeration**:
   - Checks if any RTL-SDR USB dongles are visible on the USB bus.
   - *Fix if 0 found*: Ensure the dongles are plugged into a powered hub and that host USB passthrough (e.g., `usbipd` or native USB controllers) is functioning.
2. **Device Claims & Permissions**:
   - Probes device access. If it fails with access errors:
   - *Fix*: Check if host udev rules are installed and reloaded, or if another process (like the host DVB module) is claiming the device.
3. **PPM/DC Offset Average Sanity**:
   - Performs a 0.5-second test read on each device. Computes the average DC offset for `I` and `Q` channels.
   - An offset close to `0.0` is normal. If offsets are extremely large (e.g., `>10.0`), it warns of potential hardware issues or excessive local interference.
4. **Serial Uniqueness**:
   - Ensures each connected dongle has a unique serial number in its EEPROM.
   - *Fix*: If duplicates are found, use `rtl_eeprom -s` on the host to assign unique serials (e.g., `MARINE01`, `PUBLIC01`), as duplicate serials cause role assignment conflicts in the config.
