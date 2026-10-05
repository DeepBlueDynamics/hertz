# Hertz Installation and Driver Guide

Hertz runs natively. It talks to RTL-SDR dongles through `rtl-sdr-rs` and `rusb`,
compiled with a vendored `libusb`, so there is no `librtlsdr` to install. The host
only has to let Hertz claim the USB device.

| Host OS | Driver / configuration | Use |
| :--- | :--- | :--- |
| **Linux (Pi 5 / mini-PC)** | Blacklist the kernel DVB module, install the udev rule | Production (boat) |
| **Windows** | WinUSB driver via Zadig | Development, and the validated live setup |
| **macOS** | None | Development, or a client for a remote daemon |

All hosts need a Rust toolchain (`rust-toolchain.toml` pins it) and the `whistle`
crate checked out next to this repo at `../forest/whistle` (it is a path
dependency of `hertz-daemon`).

Plug dongles into a **powered USB hub**: they draw enough current to brown out
host controllers.

---

## 1. Linux

### Step 1: Blacklist the kernel DVB driver
The kernel loads the TV tuner driver (`dvb_usb_rtl28xxu`) when a dongle is plugged
in, which blocks Hertz from claiming the interface.

```bash
echo 'blacklist dvb_usb_rtl28xxu' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
sudo modprobe -r dvb_usb_rtl28xxu    # or reboot if this fails
```

### Step 2: Install the udev rule
Lets a non-root user open RTL2832U/RTL2838 dongles:

```bash
sudo cp docs/60-hertz-rtlsdr.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

### Step 3: Build and run
```bash
cargo build --release -p hertz-daemon -p hertz-tui
cargo run --release -p hertz-sdr --example dump_fft    # prints each dongle's serial
./target/release/hertzd hertz.toml --listen
```

Start from `hertz.example.toml` and set each `[[dongle]]` `serial`. Bandplans load
from `./bandplans` (or set `HERTZ_BANDPLANS`).

---

## 2. Windows

1. Download [Zadig](https://zadig.akeo.ie/).
2. Select **Options -> List All Devices**.
3. Choose **Bulk-In, Interface (Interface 0)**.
4. Select **WinUSB** as the driver and click **Replace Driver**. (If another
   librtlsdr tool such as `rtl_fm` already works, this is done.)
5. Find the dongle's serial:
   ```powershell
   cargo run --release -p hertz-sdr --example dump_fft
   ```
   It prints `Index 0: Serial=00000001, Product=RTL2838UHIDIR` and a live spectrum.
6. Copy `hertz.windows.example.toml` to `data\hertz.windows.toml`, set your serial, and run:
   ```powershell
   cargo build --release -p hertz-daemon
   .\target\release\hertzd.exe data\hertz.windows.toml --listen
   ```

Only one program can hold a dongle at a time: stop other SDR software (for example
vhf_monitor) before starting `hertzd`.

---

## 3. macOS

No driver step. Build and run as on Linux:

```bash
cargo run --release -p hertz-daemon -- hertz.toml --listen
```

Or run `hertzd` on a Linux box on the boat network and use the Mac as a client
(section 4).

---

## 4. Client Access

The `hertz` client (crate `hertz-tui`) connects to any reachable daemon:

```bash
hertz --connect http://boat.local:9080 --token $HERTZ_TOKEN          # TUI
hertz --connect http://boat.local:9080 status                         # or doctor, records, tail, tune
```

A daemon listening on anything but localhost should set `auth_token` in `[daemon]`.

---

## 5. Transcription Engine (Cactus Whistle)

Transcription needs no install step. With `engine = "whistle"` in `[transcription]`,
the first `hertzd` start downloads the needle engine library and the 16.9 MB
`whistle.cact` weights from Hugging Face into the Whistle cache:

| OS | Cache |
| :--- | :--- |
| Windows | `%LOCALAPPDATA%\whistle` |
| Linux / macOS | `~/.cache/whistle` |

For air-gapped hosts, copy those files over and point at them with
`WHISTLE_LIB_PATH` (engine library) and `WHISTLE_WEIGHTS` (`.cact`), or move the
whole cache with `WHISTLE_CACHE`.
