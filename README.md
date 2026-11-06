# remoteswitchpro

Firmware for an ESP32-S3 (built on a WEMOS S3 mini) that plugs into a
Nintendo Switch over USB as a wired Pro Controller, and presses its buttons
on command over wifi/HTTP.

Rust, `no_std`, on esp-hal + embassy. The controller protocol is ported from
[GP2040-CE](https://github.com/OpenStickCommunity/GP2040-CE)'s Switch Pro
driver and checked against Linux's `hid-nintendo`.

On the Switch, wired controllers need **System Settings → Controllers and
Sensors → Pro Controller Wired Communication** turned on.

## Building and flashing

```sh
cp config.json.template config.json   # wifi credentials etc., gitignored
./flash.sh
```

`config.json` is baked into the firmware at build time:

| key | |
|---|---|
| `wifi.ssid`, `wifi.password` | 2.4 GHz network to join |
| `hostname` | sent with DHCP (`remoteswitchpro.home` on this network) |
| `http_port` | default 80 |
| `token` | if set, `/api/*` needs `Authorization: Bearer <token>` or `?token=<token>` |
| `log_level` | `error` … `trace` |
| `colors` | body / button / grip colours the Switch shows, `#RRGGBB` |

Once the firmware runs, the USB port belongs to the controller emulation and
the USB-Serial-JTAG console is gone. `flash.sh` handles that: it asks the
running firmware to reboot into the ROM bootloader (over USB through hidraw
when this machine is the host, else over HTTP; `RSP_HOST`, `RSP_TOKEN`,
`RSP_VIA=http`), flashes, clears the ROM's sticky force-download flag with
`esptool` (via `uvx`), and resets. If the firmware is wedged, do it by hand:
hold `0`, tap `RST`, release `0`, then `./flash.sh`.

Debug build, with the console on USB-Serial-JTAG and no controller emulation
(they share the USB pins); useful for wifi trouble:

```sh
./flash.sh --no-default-features --features jtag-log
espflash monitor
```

Logs of the normal build: `GET /api/logs` (an 8K ring buffer), or UART0 on
GPIO43.

## HTTP API

The control page at `/` has touch buttons, two sticks and keyboard keys
(arrows, `x` A, `z` B, `s` X, `a` Y, `q` L, `w` R, `1` ZL, `2` ZR, Enter +,
Backspace −, `h` Home, `c` Capture).

Buttons: `a b x y l r zl zr plus minus home capture lstick rstick up down left
right` (aliases: `+ - start select share l3 r3 …`). Wherever a list of buttons
is taken, `["a","b"]` and `"a+b"` both work. Sticks are `-1..1`, y up.

| | |
|---|---|
| `POST /api/press` | `{"buttons":["a"],"ms":100}`, or just the names as the body (`curl -d a+b`). Answers after the release. `ms` defaults to 100, at least 20. |
| `POST /api/hold` | press and keep holding |
| `POST /api/release` | release the given buttons, or all with an empty body |
| `POST /api/stick` | `{"stick":"left","x":1,"y":0}`; with `"ms"` it re-centres afterwards |
| `POST /api/sequence` | `{"steps":[{"buttons":"a","left":[0,1],"ms":100},{"ms":50}],"repeat":3}`. Each step sets the whole state (unlisted buttons up, unlisted sticks centred) for `ms`; ends neutral. Answers when done. |
| `POST /api/neutral` | everything released and centred |
| `GET /api/status` | USB state (handshake done, player LEDs the console set, …), current input, IP, last reset reason / panic |
| `GET /api/logs` | recent log lines |
| `POST /api/reboot` | |
| `POST /api/bootloader` | reboot into the ROM's download mode on USB-Serial-JTAG |

```sh
H=http://remoteswitchpro.home
curl -d a $H/api/press
curl -d '{"buttons":"l+r","ms":500}' $H/api/press        # e.g. "Change Grip/Order"
curl -d '{"stick":"left","x":0,"y":1,"ms":1000}' $H/api/stick
curl -d '{"steps":[{"buttons":"a","ms":80},{"ms":80}],"repeat":20}' $H/api/sequence
```

## How it works

- `src/usb.rs`: embassy-usb on the S3's OTG port as `057e:2009` with the Pro
  Controller's HID report descriptor. One task answers host output reports,
  another sends replies and, once the host has finished its handshake, the
  `0x30` full input report every 8 ms (paced by the host's polling). A button
  press while the bus is suspended signals remote wakeup.
- `src/switch.rs`: the protocol. USB commands (`0x80 …`), subcommands (`0x01 …`,
  answered with `0x21` reports: device info, the emulated SPI flash with
  calibration and colours, player lights, IMU/vibration enable, …) and the
  input report. The MAC is Nintendo's OUI plus the chip's MAC tail, so the
  console sees the same controller every boot.
- `src/input.rs`: the shared controller state; `src/web.rs`, `src/control.html`:
  the HTTP side; `src/net.rs`: wifi (logs the networks in range when it can't
  connect).
- `src/system.rs`: an RTC watchdog, reset-and-record on panic, and the reboot
  paths. Rebooting into download mode hands the USB PHY back to
  USB-Serial-JTAG (re-enabling it, esp-hal gates it off at boot) and stretches
  the watchdog to 30 s: espflash disables it once connected, and if nothing
  connects the board resets back into the firmware by itself.

Tested against Linux's `hid-nintendo` (full handshake, calibration, player
LED; every API call checked as input events). Not yet tested on a Switch, and
remote wakeup is untested.
