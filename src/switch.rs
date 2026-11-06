//! The Switch Pro Controller wire protocol over USB HID.
//!
//! Ported from GP2040-CE's SwitchProDriver (MIT, tested against real
//! consoles), cross-checked with Linux's hid-nintendo driver.
//!
//! Host → device (interrupt OUT, first byte is the report ID):
//! - `0x80 <cmd>`: USB-level commands (identify, handshake, baud rate,
//!   "disable USB timeout" which starts the stream of input reports).
//! - `0x01 <counter> <rumble×8> <subcmd> <args…>`: a subcommand, answered
//!   with a `0x21` report.
//! - `0x10 <counter> <rumble×8>`: rumble only, no reply.
//!
//! Device → host (interrupt IN, 64 bytes): `0x81` replies to `0x80`
//! commands, `0x21` subcommand replies, and `0x30` full input reports.

use crate::input::{self, Stick};

pub const REPORT_LEN: usize = 64;
pub type Report = [u8; REPORT_LEN];

/// Stick calibration both SPI pages advertise: centre 0x800, 0x6a4 either way.
const STICK_CENTER: f32 = 2048.0;
const STICK_RANGE: f32 = 1700.0;

/// Factory accelerometer/gyro offsets, also from the SPI page (0x6020).
const ACC_OFFSET: [i16; 3] = [-29, -199, 493];
const GYRO_OFFSET: [i16; 3] = [9, -22, -95];
/// 1G at the default ±8G range.
const ONE_G: i16 = 4096;

pub struct Protocol {
    mac: [u8; 6],
    factory_page: [u8; 256],
    /// Set by `0x80 0x04`: the host wants the input report stream.
    streaming: bool,
    timer: u8,
    player_lights: u8,
    imu_enabled: bool,
    vibration_enabled: bool,
}

#[derive(serde::Serialize, Clone, Copy, Default)]
pub struct Status {
    pub streaming: bool,
    /// Player LED pattern the host set (bits 0-3 on, 4-7 flashing).
    pub player_lights: u8,
    pub imu_enabled: bool,
    pub vibration_enabled: bool,
}

impl Protocol {
    pub fn new(mac: [u8; 6], colors: [[u8; 3]; 4]) -> Self {
        let mut factory_page = FACTORY_PAGE;
        for (i, c) in colors.iter().enumerate() {
            factory_page[0x50 + 3 * i..0x53 + 3 * i].copy_from_slice(c);
        }
        Self {
            mac,
            factory_page,
            streaming: false,
            timer: 0,
            player_lights: 0,
            imu_enabled: false,
            vibration_enabled: false,
        }
    }

    /// Back to the just-plugged-in state (USB reset or reconfiguration).
    pub fn reset(&mut self) {
        self.streaming = false;
        self.player_lights = 0;
        self.imu_enabled = false;
        self.vibration_enabled = false;
    }

    pub fn status(&self) -> Status {
        Status {
            streaming: self.streaming,
            player_lights: self.player_lights,
            imu_enabled: self.imu_enabled,
            vibration_enabled: self.vibration_enabled,
        }
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    /// Sent once after enumeration, like a real controller announcing itself.
    pub fn identify(&self) -> Report {
        let mut r = [0u8; REPORT_LEN];
        r[0] = 0x81;
        r[1] = 0x01;
        r[2] = 0x00;
        r[3] = 0x03; // pro controller
        for i in 0..6 {
            r[4 + i] = self.mac[5 - i];
        }
        r
    }

    /// Handle one output report from the host; returns the reply, if any.
    pub fn handle_output(&mut self, data: &[u8]) -> Option<Report> {
        match *data.first()? {
            0x80 => self.handle_usb_command(*data.get(1)?),
            0x01 => self.handle_subcommand(data),
            // 0x10: rumble only. Nothing to do without a motor.
            _ => None,
        }
    }

    fn handle_usb_command(&mut self, cmd: u8) -> Option<Report> {
        log::debug!("usb command 0x{cmd:02x}");
        match cmd {
            0x01 => Some(self.identify()),
            // handshake, switch to 3Mbit
            0x02 | 0x03 => {
                let mut r = [0u8; REPORT_LEN];
                r[0] = 0x81;
                r[1] = cmd;
                Some(r)
            }
            // "disable USB timeout": stay in USB mode and stream input
            // reports. A real controller doesn't reply.
            0x04 => {
                if !self.streaming {
                    log::info!("handshake complete, streaming input reports");
                }
                self.streaming = true;
                None
            }
            // "enable USB timeout"
            0x05 => None,
            _ => None,
        }
    }

    fn handle_subcommand(&mut self, data: &[u8]) -> Option<Report> {
        if data.len() < 11 {
            return None;
        }
        let cmd = data[10];
        let arg = |i: usize| data.get(11 + i).copied().unwrap_or(0);
        log::debug!("subcommand 0x{cmd:02x} {:02x?}", &data[11..data.len().min(17)]);

        let mut r = [0u8; REPORT_LEN];
        r[0] = 0x21;
        r[1] = self.next_timer();
        self.write_inputs(&mut r, &input::get());
        r[12] = 0x00;
        r[14] = cmd;

        match cmd {
            // get controller state
            0x00 => {
                r[13] = 0x80;
                r[15] = 0x03;
            }
            // bluetooth manual pairing
            0x01 => {
                r[13] = 0x81;
                r[15] = 0x03;
            }
            // device info: firmware version, type, mac, colours-in-SPI flag
            0x02 => {
                r[13] = 0x82;
                r[15] = 0x04;
                r[16] = 0x91;
                r[17] = 0x03;
                r[18] = 0x02;
                r[19..25].copy_from_slice(&self.mac);
                r[25] = 0x01;
                r[26] = 0x02;
            }
            // set input report mode; we only do 0x30 (full) anyway
            0x03 => {
                r[13] = 0x80;
                r[15] = arg(0);
            }
            // trigger buttons elapsed time
            0x04 => {
                r[13] = 0x83;
            }
            // SPI flash read
            0x10 => {
                if data.len() < 16 {
                    return None;
                }
                let addr = u32::from_le_bytes([data[11], data[12], data[13], data[14]]);
                let len = data[15] as usize;
                if len > REPORT_LEN - 20 {
                    return None;
                }
                r[13] = 0x90;
                r[15..20].copy_from_slice(&data[11..16]);
                self.read_spi(addr, &mut r[20..20 + len]);
                log::debug!("spi read 0x{addr:04x} +{len}");
            }
            0x30 => {
                self.player_lights = arg(0);
                log::info!("player lights set to 0b{:08b}", self.player_lights);
                r[13] = 0x80;
            }
            0x31 => {
                r[13] = 0xB0;
                r[15] = self.player_lights;
            }
            // chromium probing for a controller
            0x33 => {
                r[13] = 0x80;
                r[15] = 0x03;
            }
            // home light
            0x38 => {
                r[13] = 0x80;
            }
            0x40 => {
                self.imu_enabled = arg(0) != 0;
                r[13] = 0x80;
            }
            // read IMU register
            0x43 => {
                r[13] = 0xC0;
                r[15] = arg(0);
                r[16] = arg(1);
            }
            0x48 => {
                self.vibration_enabled = arg(0) != 0;
                r[13] = 0x80;
            }
            // battery voltage
            0x50 => {
                r[13] = 0xD0;
                r[15] = 0x83;
                r[16] = 0x06;
            }
            // set shipment mode, NFC/IR config and state, IMU sensitivity
            0x08 | 0x21 | 0x22 | 0x41 => {
                r[13] = 0x80;
            }
            _ => {
                log::debug!("unknown subcommand 0x{cmd:02x}, acking");
                r[13] = 0x80;
                r[15] = 0x03;
            }
        }
        Some(r)
    }

    /// The periodic `0x30` full input report.
    pub fn input_report(&mut self, state: &input::State) -> Report {
        let mut r = [0u8; REPORT_LEN];
        r[0] = 0x30;
        r[1] = self.next_timer();
        self.write_inputs(&mut r, state);
        r[12] = 0x09;
        if self.imu_enabled {
            // three samples of a controller lying still and flat
            let acc = [ACC_OFFSET[0], ACC_OFFSET[1], ACC_OFFSET[2] + ONE_G];
            for frame in 0..3 {
                let base = 13 + frame * 12;
                for axis in 0..3 {
                    r[base + 2 * axis..base + 2 * axis + 2]
                        .copy_from_slice(&acc[axis].to_le_bytes());
                    r[base + 6 + 2 * axis..base + 8 + 2 * axis]
                        .copy_from_slice(&GYRO_OFFSET[axis].to_le_bytes());
                }
            }
        }
        r
    }

    fn next_timer(&mut self) -> u8 {
        self.timer = self.timer.wrapping_add(1);
        self.timer
    }

    /// Bytes 2..=11: battery/connection, three button bytes, two sticks.
    fn write_inputs(&self, r: &mut Report, state: &input::State) {
        // battery full, charging, powered by the host
        r[2] = 0x91;
        // bit 7 of the middle byte: "charging grip", as GP2040 sets it
        let buttons = state.buttons | (1 << 15);
        r[3] = buttons as u8;
        r[4] = (buttons >> 8) as u8;
        r[5] = (buttons >> 16) as u8;
        r[6..9].copy_from_slice(&pack_stick(state.left));
        r[9..12].copy_from_slice(&pack_stick(state.right));
    }

    fn read_spi(&self, addr: u32, out: &mut [u8]) {
        let page: &[u8] = match addr & !0xFF {
            0x6000 => &self.factory_page,
            0x8000 => &USER_PAGE,
            _ => &[],
        };
        let offset = (addr & 0xFF) as usize;
        match page.get(offset..offset + out.len()) {
            Some(src) => out.copy_from_slice(src),
            None => out.fill(0xFF),
        }
    }
}

fn axis(v: f32) -> u16 {
    let raw = STICK_CENTER + v * STICK_RANGE;
    let rounded = if raw >= 0.0 { raw + 0.5 } else { raw - 0.5 };
    (rounded as i32).clamp(0, 0xFFF) as u16
}

/// Two 12-bit values in three bytes.
fn pack_stick(s: Stick) -> [u8; 3] {
    let x = axis(s.x);
    let y = axis(s.y);
    [x as u8, ((x >> 8) as u8 & 0x0F) | ((y as u8 & 0x0F) << 4), (y >> 4) as u8]
}

/// SPI flash 0x6000..0x60FF: serial, device type, colours, factory
/// calibration. Byte for byte GP2040-CE's, which consoles accept.
const FACTORY_PAGE: [u8; 256] = {
    let mut p = [0u8; 256];
    let mut i = 0;
    while i < 0x12 {
        p[i] = 0xFF; // serial number (none)
        i += 1;
    }
    p[0x12] = 0x03; // pro controller
    p[0x13] = 0xA0;
    let mut i = 0x14;
    while i < 0x1B {
        p[i] = 0xFF;
        i += 1;
    }
    p[0x1B] = 0x02; // colours: use the ones below
    p[0x1C] = 0xFF;
    p[0x1D] = 0xFF;
    p[0x1E] = 0xFF;
    p[0x1F] = 0xFF;
    // 0x6020: IMU factory calibration
    let imu: [u8; 24] = [
        0xE3, 0xFF, 0x39, 0xFF, 0xED, 0x01, 0x00, 0x40, 0x00, 0x40, 0x00, 0x40, 0x09, 0x00, 0xEA,
        0xFF, 0xA1, 0xFF, 0x3B, 0x34, 0x3B, 0x34, 0x3B, 0x34,
    ];
    let mut i = 0;
    while i < 24 {
        p[0x20 + i] = imu[i];
        i += 1;
    }
    let mut i = 0x38;
    while i < 0x3D {
        p[i] = 0xFF;
        i += 1;
    }
    // 0x603D: left stick (max offset, centre, min offset); 0x6046: right
    // stick (centre, min offset, max offset)
    let sticks: [u8; 18] = [
        0xA4, 0x46, 0x6A, 0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A, 0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A,
        0xA4, 0x46, 0x6A,
    ];
    let mut i = 0;
    while i < 18 {
        p[0x3D + i] = sticks[i];
        i += 1;
    }
    p[0x4F] = 0xFF;
    // 0x6050..0x605B: body, buttons, left grip, right grip colours (filled
    // in from the config)
    p[0x5C] = 0x01;
    let mut i = 0x5D;
    while i < 0x80 {
        p[i] = 0xFF;
        i += 1;
    }
    // 0x6080: IMU horizontal offsets, then stick parameters for both sticks
    let params: [u8; 30] = [
        0x50, 0xFD, 0x00, 0x00, 0xC6, 0x0F, 0x0F, 0x30, 0x61, 0xAE, 0x90, 0xD9, 0xD4, 0x14, 0x54,
        0x41, 0x15, 0x54, 0xC7, 0x79, 0x9C, 0x33, 0x36, 0x63, 0x0F, 0x30, 0x61, 0xAE, 0x90, 0xD9,
    ];
    let mut i = 0;
    while i < 30 {
        p[0x80 + i] = params[i];
        i += 1;
    }
    let params2: [u8; 18] = [
        0xD4, 0x14, 0x54, 0x41, 0x15, 0x54, 0xC7, 0x79, 0x9C, 0x33, 0x36, 0x63, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF,
    ];
    let mut i = 0;
    while i < 18 {
        p[0x9E + i] = params2[i];
        i += 1;
    }
    p
};

/// SPI flash 0x8000..0x80FF: user calibration. Stick calibration with the
/// 0xB2 0xA1 magic (same values as factory), no user IMU calibration.
const USER_PAGE: [u8; 256] = {
    let mut p = [0xFFu8; 256];
    let cal: [u8; 22] = [
        0xB2, 0xA1, 0xA4, 0x46, 0x6A, 0x00, 0x08, 0x80, 0xA4, 0x46, 0x6A, 0xB2, 0xA1, 0x00, 0x08,
        0x80, 0xA4, 0x46, 0x6A, 0xA4, 0x46, 0x6A,
    ];
    let mut i = 0;
    while i < 22 {
        p[0x10 + i] = cal[i];
        i += 1;
    }
    p
};
