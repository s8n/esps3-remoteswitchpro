//! Logging to UART0 (GPIO43) and to a RAM ring buffer served at /api/logs.
//!
//! Once the USB OTG controller takes over the USB pins, the USB-Serial-JTAG
//! console is gone, so the ring buffer is the practical way to read logs.

use alloc::string::String;
use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use heapless::Deque;

const RING_SIZE: usize = 8 * 1024;

static RING: Mutex<RefCell<Deque<u8, RING_SIZE>>> = Mutex::new(RefCell::new(Deque::new()));

struct Logger;

static LOGGER: Logger = Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // picoserve logs every connection at info
        if metadata.target().starts_with("picoserve") {
            return metadata.level() <= log::Level::Warn;
        }
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ms = embassy_time::Instant::now().as_millis();
        let mut line = String::new();
        let _ = writeln!(
            line,
            "[{:>6}.{:03} {:<5} {}] {}",
            ms / 1000,
            ms % 1000,
            record.level(),
            record.target(),
            record.args()
        );
        esp_println::print!("{line}");
        critical_section::with(|cs| {
            let mut ring = RING.borrow_ref_mut(cs);
            for &b in line.as_bytes() {
                if ring.is_full() {
                    ring.pop_front();
                }
                let _ = ring.push_back(b);
            }
        });
    }

    fn flush(&self) {}
}

pub fn init(level: log::LevelFilter) {
    // SAFETY: called once at boot, before any other task exists
    unsafe {
        let _ = log::set_logger_racy(&LOGGER);
        log::set_max_level_racy(level);
    }
}

/// The ring buffer's contents, starting at the first complete line.
pub fn snapshot() -> String {
    let bytes: alloc::vec::Vec<u8> =
        critical_section::with(|cs| RING.borrow_ref(cs).iter().copied().collect());
    let start = if bytes.len() == RING_SIZE {
        bytes.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)
    } else {
        0
    };
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
