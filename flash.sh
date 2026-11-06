#!/usr/bin/env bash
# Build and flash, getting the board into download mode without touching it.
#
#   ./flash.sh                       # build + flash the normal firmware
#   ./flash.sh --no-default-features --features jtag-log   # debug build
#
# If the firmware is running (it owns the USB port as a Pro Controller), it
# is asked to reboot into the ROM bootloader: over USB via hidraw when this
# machine is the host, otherwise over HTTP (RSP_HOST, default
# remoteswitchpro.home; RSP_TOKEN if the API has a token; RSP_VIA=http forces
# HTTP). The ROM's
# force-download flag survives resets, so it is cleared with esptool (run
# through uvx) before the final reset.
set -euo pipefail
cd "$(dirname "$0")"

[ -f ~/export-esp.sh ] && . ~/export-esp.sh
cargo build --release "$@"
ELF=target/xtensa-esp32s3-none-elf/release/remoteswitchpro
HOST=${RSP_HOST:-remoteswitchpro.home}

find_port() {
    for p in /dev/ttyACM*; do
        [ -e "$p" ] && udevadm info "$p" 2>/dev/null | grep -q ID_VENDOR=Espressif && { echo "$p"; return; }
    done
    true
}

find_hidraw() {
    for d in /sys/class/hidraw/hidraw*; do
        [ -e "$d" ] || continue
        grep -q "HID_ID=0003:0000057E:00002009" "$d/device/uevent" 2>/dev/null || continue
        grep -q "HID_NAME=.*remoteswitchpro" "$d/device/uevent" 2>/dev/null || continue
        echo "/dev/$(basename "$d")"
        return
    done
    true
}

PORT=$(find_port)
if [ -z "$PORT" ]; then
    HIDRAW=""
    [ "${RSP_VIA:-}" = http ] || HIDRAW=$(find_hidraw)
    if [ -n "$HIDRAW" ] && [ -w "$HIDRAW" ]; then
        echo "asking the firmware to enter the bootloader via $HIDRAW"
        # report 0x82 + magic, padded to the 64 byte report size
        { printf '\x82rswp-bootloader'; head -c 48 /dev/zero; } > "$HIDRAW"
    else
        echo "asking the firmware to enter the bootloader via http://$HOST"
        curl -fsS -X POST ${RSP_TOKEN:+-H "Authorization: Bearer $RSP_TOKEN"} "http://$HOST/api/bootloader"
    fi
    for _ in $(seq 1 20); do
        sleep 1
        PORT=$(find_port)
        [ -n "$PORT" ] && break
    done
    [ -n "$PORT" ] || { echo "no USB-Serial-JTAG port appeared; hold BOOT (0), tap RST, release BOOT and retry"; exit 1; }
fi

echo "flashing via $PORT"
espflash flash --port "$PORT" --chip esp32s3 "$ELF"
# clear RTC_CNTL_OPTION1.FORCE_DOWNLOAD_BOOT so the reset boots the app
uvx esptool --chip esp32s3 --port "$PORT" --before no-reset --after no-reset \
    write-mem 0x6000812C 0 >/dev/null
espflash reset --port "$PORT" --chip esp32s3
