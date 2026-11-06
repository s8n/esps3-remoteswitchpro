//! Resets, the watchdog, and remembering why the last boot ended.
//!
//! - A panic is logged, recorded in RTC fast RAM (which survives a software
//!   reset) and followed by a reset, instead of halting forever. The next
//!   boot reports it in /api/status.
//! - The RTC watchdog resets the chip if the executor stops running the feed
//!   task for [`WATCHDOG_TIMEOUT`].
//! - [`reboot_to_bootloader`] gets the chip back into the ROM's serial
//!   download mode for flashing. With the OTG controller owning the USB pins
//!   there is no USB-Serial-JTAG for espflash to reset the chip through.

use core::fmt::Write as _;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::string::String;
use embassy_sync::once_lock::OnceLock;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::{LPWR, USB_WRAP};
use esp_hal::rtc_cntl::{Rtc, RwdtStage};

pub const WATCHDOG_TIMEOUT: esp_hal::time::Duration = esp_hal::time::Duration::from_secs(10);

const MAGIC: u32 = 0x7273_7770; // "rswp"
const MESSAGE_LEN: usize = 384;

#[repr(C)]
struct Record {
    magic: u32,
    message_len: u32,
    message: [u8; MESSAGE_LEN],
}

// SAFETY: plain integers and a byte array, valid for any bit pattern.
unsafe impl esp_hal::Persistable for Record {}

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut RECORD: Record = Record {
    magic: 0,
    message_len: 0,
    message: [0; MESSAGE_LEN],
};

#[derive(serde::Serialize)]
pub struct LastReset {
    pub reason: String,
    /// The panic message and backtrace addresses (resolve with
    /// `xtensa-esp32s3-elf-addr2line -e target/.../remoteswitchpro`).
    pub panic: Option<String>,
}

static LAST_RESET: OnceLock<LastReset> = OnceLock::new();

pub fn last_reset() -> Option<&'static LastReset> {
    LAST_RESET.try_get()
}

/// Collect what the previous boot left behind. Call once, before any task.
pub fn init() {
    // a download-mode reboot that ended in a watchdog reset leaves this set
    LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().clear_bit());
    // SAFETY: only called once, from main before any task runs
    let record = unsafe { &mut *addr_of_mut!(RECORD) };
    let panic = (record.magic == MAGIC && record.message_len > 0).then(|| {
        let bytes = &record.message[..(record.message_len as usize).min(MESSAGE_LEN)];
        String::from_utf8_lossy(bytes).into_owned()
    });
    record.magic = MAGIC;
    record.message_len = 0;

    let reason = match esp_hal::system::reset_reason() {
        Some(reason) => alloc::format!("{reason:?}"),
        None => "unknown".into(),
    };
    match &panic {
        Some(p) => log::warn!("previous boot panicked: {p}"),
        None => log::info!("reset reason: {reason}"),
    }
    let _ = LAST_RESET.init(LastReset { reason, panic });
}

/// Arm the RTC watchdog and keep feeding it from a task.
pub fn start_watchdog(spawner: embassy_executor::Spawner, mut rtc: Rtc<'static>) {
    rtc.rwdt.set_timeout(RwdtStage::Stage0, WATCHDOG_TIMEOUT);
    rtc.rwdt.enable();
    rtc.rwdt.feed();
    spawner.spawn(watchdog_task(rtc).unwrap());
    spawner.spawn(reboot_task().unwrap());
}

#[embassy_executor::task]
async fn watchdog_task(mut rtc: Rtc<'static>) {
    loop {
        rtc.rwdt.feed();
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// OTG core device control register (DCTL) and its soft-disconnect bit.
const OTG_DCTL: *mut u32 = (0x6008_0000 + 0x804) as *mut u32;
const DCTL_SFTDISCON: u32 = 1 << 1;

/// Drop off the bus and reset. With `download`, the ROM stays in its serial
/// download mode.
///
/// Runs with interrupts masked. The RTC watchdog keeps running through the
/// reset (the ROM never feeds it), so before a download-mode reset it is
/// stretched to 30s: long enough for espflash to connect (it disables the
/// watchdog once connected), short enough that a download mode nobody can
/// reach ends in a reset back into this firmware.
fn reset_releasing_usb(download: bool) -> ! {
    critical_section::with(|_| {
        if download {
            // SAFETY: only the watchdog is touched; the owning Rtc in the
            // feed task never runs again with interrupts masked
            let mut rtc = Rtc::new(unsafe { LPWR::steal() });
            rtc.rwdt.set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_secs(30));
            rtc.rwdt.feed();
            LPWR::regs()
                .option1()
                .modify(|_, w| w.force_download_boot().set_bit());
        }
        // SAFETY: DCTL is a plain read-write register of the OTG core, which
        // nothing else touches any more with interrupts masked
        unsafe { OTG_DCTL.write_volatile(OTG_DCTL.read_volatile() | DCTL_SFTDISCON) };
        USB_WRAP::regs()
            .otg_conf()
            .modify(|_, w| w.usb_pad_enable().clear_bit());
        // give the host time to notice the disconnect
        esp_hal::delay::Delay::new().delay_millis(200);
        LPWR::regs().usb_conf().modify(|_, w| {
            w.sw_hw_usb_phy_sel().clear_bit();
            w.sw_usb_phy_sel().clear_bit()
        });
        if download {
            // esp-hal gates USB-Serial-JTAG off at boot when unused
            let system = esp_hal::peripherals::SYSTEM::regs();
            system
                .perip_clk_en1()
                .modify(|_, w| w.usb_device_clk_en().set_bit());
            system
                .perip_rst_en1()
                .modify(|_, w| w.usb_device_rst().clear_bit());
            esp_hal::peripherals::USB_DEVICE::regs().conf0().modify(|_, w| {
                w.pad_pull_override().clear_bit();
                w.dp_pullup().set_bit();
                w.usb_pad_enable().set_bit()
            });
        }
        esp_hal::system::software_reset()
    })
}

#[derive(Clone, Copy)]
pub enum RebootKind {
    Normal,
    Bootloader,
}

static REBOOT: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    RebootKind,
> = embassy_sync::signal::Signal::new();

/// Reboot shortly, once an HTTP response has had a moment to go out.
pub fn request_reboot(kind: RebootKind) {
    REBOOT.signal(kind);
}

#[embassy_executor::task]
async fn reboot_task() {
    let kind = REBOOT.wait().await;
    Timer::after(Duration::from_millis(500)).await;
    match kind {
        RebootKind::Normal => reboot(),
        RebootKind::Bootloader => reboot_to_bootloader(),
    }
}

pub fn reboot() -> ! {
    log::info!("rebooting");
    reset_releasing_usb(false)
}

/// Reset into the ROM's download mode, reachable over USB-Serial-JTAG
/// (303a:1001), where `espflash flash` works as usual.
///
/// The force-download flag sits in the RTC domain and survives resets, so the
/// chip keeps coming back to download mode until it is cleared (flash.sh does
/// that with `esptool write-mem 0x6000812C 0`) or the board is power cycled.
pub fn reboot_to_bootloader() -> ! {
    log::info!("rebooting into the ROM bootloader");
    reset_releasing_usb(true)
}

/// Writes into a fixed buffer, dropping whatever doesn't fit. Can't
/// allocate: the panic may well be an allocation failure.
struct Truncating<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for Truncating<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if PANICKING.swap(true, Ordering::Relaxed) {
        esp_hal::system::software_reset();
    }

    esp_println::println!("\n====================== PANIC ======================");
    esp_println::println!("{info}\n\nBacktrace:\n");
    let backtrace = esp_backtrace::Backtrace::capture();
    for frame in backtrace.frames() {
        esp_println::println!("0x{:x}", frame.program_counter());
    }

    // SAFETY: the panic handler runs once (see PANICKING) and never returns
    let record = unsafe { &mut *addr_of_mut!(RECORD) };
    let mut writer = Truncating {
        buf: &mut record.message,
        len: 0,
    };
    let _ = write!(writer, "{info} | backtrace:");
    for frame in backtrace.frames() {
        let _ = write!(writer, " 0x{:x}", frame.program_counter());
    }
    let len = writer.len;
    unsafe {
        addr_of_mut!(RECORD.message_len).write_volatile(len as u32);
        let _ = addr_of!(RECORD.message).read_volatile();
    }

    esp_println::println!("\nresetting");
    reset_releasing_usb(false)
}
