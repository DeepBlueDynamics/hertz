# Hertz Installation and Driver Matrix Guide

This guide covers installing and configuring the driver layer and containerized runtime for the Hertz SDR receiver stack.

## Driver Installation & Compatibility Matrix

Hertz uses `rtl-sdr-rs` (which relies on `rusb`) for direct USB communication. It compiles with a statically-linked, vendored `libusb`, meaning the runtime container has no external library requirements. However, the host operating system must be configured to allow USB device access.

| Host OS | USB Passthrough Support | Driver / Configuration Required | Recommended Use |
| :--- | :--- | :--- | :--- |
| **Linux (Pi 5 / Mini-PC)** | **Native (Full)** | Blacklist kernel DVB module, install udev rules | Production (Supported Boat Deployment) |
| **Windows (WSL2)** | **Via usbipd-win** | Install `usbipd-win`, bind & attach device to WSL2 | Development & Testing |
| **Windows (Native)** | **Native** | Run natively using Zadig (WinUSB driver) | Dev Fallback (Non-Containerized) |
| **macOS** | **No Container USB** | Run natively via cargo or run remotely | Remote Client / Dev |

---

## 1. Linux Host Installation (Production Boat Deployment)

This is the primary supported deployment environment (e.g., Raspberry Pi 5, Intel NUC, or any Linux-based onboard PC).

### Step 1: Blacklist the Kernel DVB Driver
By default, the Linux kernel loads the TV tuner driver (`dvb_usb_rtl28xxu`) when an RTL-SDR dongle is plugged in. This blocks Hertz from claiming the USB interface.

1. Blacklist the kernel module:
   ```bash
   echo 'blacklist dvb_usb_rtl28xxu' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
   ```
2. Unload the module if currently loaded:
   ```bash
   sudo modprobe -r dvb_usb_rtl28xxu
   ```
   *(Note: If unloading fails, reboot the host).*

### Step 2: Configure udev Permissions
To allow the container's non-root user (`hertz`) to access the USB dongles:

1. Copy the udev rules file (included in `docker/60-hertz-rtlsdr.rules`):
   ```bash
   sudo cp docker/60-hertz-rtlsdr.rules /etc/udev/rules.d/
   ```
2. Reload and trigger udev:
   ```bash
   sudo udevadm control --reload && sudo udevadm trigger
   ```

### Step 3: Run the Container Stack
1. Connect your RTL-SDR dongles to a **powered USB hub** (dongles draw significant current and can brown-out host controllers).
2. Start the container stack:
   ```bash
   docker compose -f docker/compose.yaml up -d
   ```
3. Run first-run diagnostics:
   ```bash
   docker exec -it hertzd hertz doctor
   ```

---

## 2. Windows Host Installation (WSL2 Developer Flow)

Docker Desktop on Windows does not support direct USB passthrough. To deploy Hertz in a container on Windows, you must use the `usbipd-win` tool to attach the USB device to your WSL2 Linux kernel.

### Step 1: Install usbipd-win
1. Open PowerShell as Administrator and run:
   ```powershell
   winget install usbipd
   ```
2. Close and reopen the terminal to reload environment paths.

### Step 2: Bind and Attach the Dongle
1. List available USB devices:
   ```powershell
   usbipd list
   ```
   Find the entry corresponding to your RTL-SDR device (usually listed as "Realtek RTL2832U" or "Bulk-In, Interface"). Note its Bus ID (e.g. `2-3`).
2. Bind the device (requires Administrator privileges):
   ```powershell
   usbipd bind --busid <BUSID>
   ```
3. Attach the device to WSL2 (run while your WSL2 distribution is running):
   ```powershell
   usbipd attach --wsl --busid <BUSID>
   ```
   > [!TIP]
   > Use `usbipd attach --wsl --busid <BUSID> --auto-attach` to automatically attach the device if it is unplugged and replugged.

### Step 3: Configure WSL2 Linux
1. Open your WSL2 terminal.
2. Apply the Linux udev rules and blacklist instructions described in the **Linux Host** section.
3. Start the Docker containers inside WSL2.

### Native Windows Fallback (Non-Containerized)
For local development, you can run the Hertz binaries directly on Windows:
1. Download [Zadig](https://zadig.akeo.ie/).
2. Select **Options -> List All Devices**.
3. Choose **Bulk-In, Interface (Interface 0)**.
4. Select **WinUSB** as the driver and click **Replace Driver**.
5. Run the daemon via `cargo run --package hertz-daemon`.

---

## 3. macOS Host Installation

macOS does not support containerized USB passthrough. 

### Recommended Flow:
- **Run Natively**: Run the daemon natively on macOS:
  ```bash
  cargo run --release -p hertz-daemon -- --config hertz.toml
  ```
  *(Note: Because Hertz uses a pure-Rust driver, no external C library or homebrew `librtlsdr` installation is required).*
- **Run Remotely**: Deploy the daemon container on a Linux box (e.g., connected to the boat's network), and run the TUI locally connecting to the remote instance:
  ```bash
  hertz tui --connect http://boat.local:9080 --token $HERTZ_TOKEN
  ```

---

## 4. TUI Access Invocations

### On the Docker Host
Run the interactive TUI directly inside the running container:
```bash
docker exec -it hertzd hertz
```

### Remote TUI Access
Connect to a remote Hertz daemon from another computer or run:
```bash
docker run --rm -it ghcr.io/deepbluedynamics/hertz hertz --connect http://<IP>:9080 --token $HERTZ_TOKEN
```
