//! remoteswitchpro: an ESP32-S3 that enumerates as a Nintendo Switch Pro
//! Controller over USB and takes its button presses over HTTP.

#![no_std]
#![no_main]
// the picoserve router's nested future types are deep
#![recursion_limit = "512"]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
// jtag-log builds leave the controller emulation unused
#![cfg_attr(feature = "jtag-log", allow(dead_code, unused_imports))]

extern crate alloc;

mod config;
mod input;
mod logger;
mod net;
mod switch;
mod system;
mod usb;
mod web;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 72 * 1024);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    config::init();
    let config = config::get();
    logger::init(match config.log_level.as_str() {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    });
    log::info!("remoteswitchpro {} starting", env!("CARGO_PKG_VERSION"));

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    system::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    system::start_watchdog(spawner, Rtc::new(peripherals.LPWR));

    // Nintendo's OUI plus the chip's own MAC tail: stable across boots, so
    // the console keeps recognising the same controller.
    let base_mac = esp_hal::efuse::base_mac_address();
    let base_mac = base_mac.as_bytes();
    let mac = [0x7C, 0xBB, 0x8A, base_mac[3], base_mac[4], base_mac[5]];

    let colors = [
        &config.colors.body,
        &config.colors.buttons,
        &config.colors.left_grip,
        &config.colors.right_grip,
    ]
    .map(|c| {
        config::parse_color(c).unwrap_or_else(|| {
            log::warn!("invalid colour {c:?}, using grey");
            [0x80, 0x80, 0x80]
        })
    });

    let protocol = switch::Protocol::new(mac, colors);
    // jtag-log builds keep the USB pins on USB-Serial-JTAG for the console
    #[cfg(not(feature = "jtag-log"))]
    usb::init(
        spawner,
        peripherals.USB0,
        peripherals.GPIO20,
        peripherals.GPIO19,
        protocol,
    );
    #[cfg(feature = "jtag-log")]
    {
        let _ = protocol;
        log::warn!("jtag-log build: not starting the USB controller");
    }

    if let Some(stack) = net::init(spawner, peripherals.WIFI) {
        web::spawn(spawner, stack);
    }

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
