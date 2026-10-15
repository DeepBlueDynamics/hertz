# @deepbluedynamics/hertz

Hertz is a scanning radio receiver for RTL-SDR dongles. It scans marine VHF and
other band plans, records transmissions, and transcribes them on-device. This
package installs two prebuilt programs:

- `hertzd`: the daemon. It drives the dongles, records and transcribes.
- `hertz`: the operator console and command-line client.

```sh
npm install -g @deepbluedynamics/hertz
hertzd --version
```

Supported: Linux arm64 (Raspberry Pi 4/5, HaLOS), Linux x64, macOS (Apple Silicon
and Intel), Windows x64. npm installs only the binary for your machine.

## Run

```sh
cp "$(npm root -g)/@deepbluedynamics/hertz/hertz.example.toml" hertz.toml   # then edit
hertzd hertz.toml --listen
hertz           # console, connects to http://localhost:9080
```

The band plans ship with this package; set `HERTZ_BANDPLANS` to use your own.
On first use, hertzd downloads the Whistle speech engine and weights (about 17 MB)
from Hugging Face and caches them, so run it once with internet before going
offline.

## Linux and Raspberry Pi: one-time USB setup

npm can't do this part. Install the two system libraries hertz uses (most Pi
images already have them), stop the kernel's TV driver from claiming the dongle,
and allow your user to open it:

```sh
sudo apt install libasound2 libudev1
echo 'blacklist dvb_usb_rtl28xxu' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
sudo curl -o /etc/udev/rules.d/60-hertz-rtlsdr.rules \
  https://raw.githubusercontent.com/DeepBlueDynamics/hertz/main/docs/60-hertz-rtlsdr.rules
sudo udevadm control --reload && sudo udevadm trigger
```

Then unplug and replug the dongle (or reboot). Use a powered USB hub.

Source and documentation: [DeepBlueDynamics/hertz](https://github.com/DeepBlueDynamics/hertz).
