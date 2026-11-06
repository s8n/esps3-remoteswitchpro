//! The USB side: enumerate as a Pro Controller (057e:2009) on the S3's OTG
//! port, answer the host's handshake and subcommands, and stream input
//! reports from `input::get()`.

use core::cell::{Cell, RefCell};

use critical_section::Mutex;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use embassy_usb::class::hid::{self, HidReader, HidReaderWriter, HidWriter, ReadError, ReportId};
use embassy_usb::control::OutResponse;
use embassy_usb::{Builder, Handler, UsbDevice, UsbVersion};
use esp_hal::otg_fs::Usb;
use esp_hal::otg_fs::asynch::{Config as OtgConfig, Driver};
use static_cell::StaticCell;

use crate::input;
use crate::switch::{self, Protocol, Report};

type UsbDriver = Driver<'static>;

static PROTOCOL: Mutex<RefCell<Option<Protocol>>> = Mutex::new(RefCell::new(None));
/// Replies to host commands, sent ahead of the next input report.
static REPLIES: Channel<CriticalSectionRawMutex, Report, 4> = Channel::new();

#[derive(Clone, Copy, Default)]
struct BusState {
    configured: bool,
    suspended: bool,
}

static BUS: Mutex<Cell<BusState>> = Mutex::new(Cell::new(BusState {
    configured: false,
    suspended: false,
}));

#[derive(serde::Serialize)]
pub struct Status {
    pub configured: bool,
    pub suspended: bool,
    #[serde(flatten)]
    pub protocol: switch::Status,
}

pub fn status() -> Status {
    let bus = critical_section::with(|cs| BUS.borrow(cs).get());
    // None in jtag-log builds, which never start the USB side
    let protocol = critical_section::with(|cs| PROTOCOL.borrow_ref(cs).as_ref().map(|p| p.status()))
        .unwrap_or_default();
    Status {
        configured: bus.configured,
        suspended: bus.suspended,
        protocol,
    }
}

fn with_protocol<R>(f: impl FnOnce(&mut Protocol) -> R) -> R {
    critical_section::with(|cs| {
        f(PROTOCOL
            .borrow_ref_mut(cs)
            .as_mut()
            .expect("protocol initialized before the usb tasks"))
    })
}

/// HID report descriptor of a Pro Controller (GP2040-CE's copy).
const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x15, 0x00, 0x09, 0x04, 0xA1, 0x01, // joystick, application collection
    // report 0x30: buttons, sticks, hat
    0x85, 0x30, 0x05, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0A, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x0A, 0x55, 0x00, 0x65, 0x00, 0x81, 0x02, 0x05, 0x09, 0x19, 0x0B, 0x29, 0x0E,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02, 0x75, 0x01, 0x95, 0x02, 0x81,
    0x03, 0x0B, 0x01, 0x00, 0x01, 0x00, 0xA1, 0x00, 0x0B, 0x30, 0x00, 0x01, 0x00, 0x0B, 0x31,
    0x00, 0x01, 0x00, 0x0B, 0x32, 0x00, 0x01, 0x00, 0x0B, 0x35, 0x00, 0x01, 0x00, 0x15, 0x00,
    0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, 0xC0, 0x0B, 0x39, 0x00,
    0x01, 0x00, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04,
    0x95, 0x01, 0x81, 0x02, 0x05, 0x09, 0x19, 0x0F, 0x29, 0x12, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x04, 0x81, 0x02, 0x75, 0x08, 0x95, 0x34, 0x81, 0x03,
    0x06, 0x00, 0xFF, // vendor page
    0x85, 0x21, 0x09, 0x01, 0x75, 0x08, 0x95, 0x3F, 0x81, 0x03, // in 0x21
    0x85, 0x81, 0x09, 0x02, 0x75, 0x08, 0x95, 0x3F, 0x81, 0x03, // in 0x81
    0x85, 0x01, 0x09, 0x03, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, // out 0x01
    0x85, 0x10, 0x09, 0x04, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, // out 0x10
    0x85, 0x80, 0x09, 0x05, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, // out 0x80
    0x85, 0x82, 0x09, 0x06, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, // out 0x82
    0xC0,
];

struct DeviceHandler;

impl Handler for DeviceHandler {
    fn reset(&mut self) {
        log::debug!("usb reset");
        // embassy-usb leaves suspend on a reset without calling suspended()
        critical_section::with(|cs| {
            let cell = BUS.borrow(cs);
            cell.set(BusState { suspended: false, ..cell.get() });
        });
        with_protocol(|p| p.reset());
        REPLIES.clear();
    }

    fn configured(&mut self, configured: bool) {
        log::info!("usb {}", if configured { "configured" } else { "unconfigured" });
        critical_section::with(|cs| {
            let cell = BUS.borrow(cs);
            cell.set(BusState { configured, ..cell.get() });
        });
        if !configured {
            with_protocol(|p| p.reset());
        }
    }

    fn suspended(&mut self, suspended: bool) {
        log::info!("usb {}", if suspended { "suspended" } else { "resumed" });
        critical_section::with(|cs| {
            let cell = BUS.borrow(cs);
            cell.set(BusState { suspended, ..cell.get() });
        });
    }
}

/// Output reports sent over the control pipe (SET_REPORT) instead of the
/// interrupt endpoint end up here; handled the same way.
struct ControlReports;

impl hid::RequestHandler for ControlReports {
    fn set_report(&mut self, id: ReportId, data: &[u8]) -> OutResponse {
        log::debug!("SET_REPORT {id:?} over the control pipe");
        handle_output(data);
        OutResponse::Accepted
    }
}

/// Output report 0x82 (declared in the descriptor, never sent by a console)
/// followed by this reboots into the ROM bootloader, so a host the device is
/// plugged into can re-flash it without pressing buttons:
/// `printf '\x82rswp-bootloader' > /dev/hidrawN`
const BOOTLOADER_MAGIC: &[u8] = b"\x82rswp-bootloader";

fn handle_output(data: &[u8]) {
    if data.starts_with(BOOTLOADER_MAGIC) {
        crate::system::request_reboot(crate::system::RebootKind::Bootloader);
        return;
    }
    if let Some(reply) = with_protocol(|p| p.handle_output(data))
        && REPLIES.try_send(reply).is_err()
    {
        log::warn!("reply queue full, dropping a reply");
    }
}

pub fn init(
    spawner: embassy_executor::Spawner,
    usb0: esp_hal::peripherals::USB0<'static>,
    dp: esp_hal::peripherals::GPIO20<'static>,
    dm: esp_hal::peripherals::GPIO19<'static>,
    protocol: Protocol,
) {
    critical_section::with(|cs| PROTOCOL.borrow_ref_mut(cs).replace(protocol));

    static EP_OUT_BUFFER: StaticCell<[u8; 1024]> = StaticCell::new();
    let driver = Driver::new(
        Usb::new(usb0, dp, dm),
        EP_OUT_BUFFER.init([0; 1024]),
        OtgConfig::default(),
    );

    let mut config = embassy_usb::Config::new(0x057E, 0x2009);
    config.bcd_usb = UsbVersion::Two;
    config.device_class = 0x00;
    config.device_sub_class = 0x00;
    config.device_protocol = 0x00;
    config.composite_with_iads = false;
    config.device_release = 0x0210;
    config.max_packet_size_0 = 64;
    config.manufacturer = Some("remoteswitchpro");
    config.product = Some("Pro Controller (remoteswitchpro)");
    config.serial_number = Some("000000000001");
    config.self_powered = false;
    config.max_power = 500;
    config.supports_remote_wakeup = true;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 64]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    static HANDLER: StaticCell<DeviceHandler> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 64]),
        &mut [],
        CONTROL_BUF.init([0; 128]),
    );
    builder.handler(HANDLER.init(DeviceHandler));

    static HID_STATE: StaticCell<hid::State> = StaticCell::new();
    static CONTROL_REPORTS: StaticCell<ControlReports> = StaticCell::new();
    let hid = HidReaderWriter::<_, 64, 64>::new(
        &mut builder,
        HID_STATE.init(hid::State::new()),
        hid::Config {
            report_descriptor: REPORT_DESCRIPTOR,
            request_handler: Some(CONTROL_REPORTS.init(ControlReports)),
            poll_ms: 8,
            max_packet_size: 64,
        },
    );
    let (reader, writer) = hid.split();

    spawner.spawn(usb_task(builder.build()).unwrap());
    spawner.spawn(reader_task(reader).unwrap());
    spawner.spawn(writer_task(writer).unwrap());
}

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, UsbDriver>) {
    loop {
        usb.run_until_suspend().await;
        // a button press on a sleeping console wakes it, like a real
        // controller's HOME button
        input::PRESSED.reset();
        if let Either::Second(()) = select(usb.wait_resume(), input::PRESSED.wait()).await {
            log::info!("button pressed while suspended, signalling remote wakeup");
            if let Err(e) = usb.remote_wakeup().await {
                log::warn!("remote wakeup failed: {e:?}");
            }
        }
    }
}

#[embassy_executor::task]
async fn reader_task(mut reader: HidReader<'static, UsbDriver, 64>) {
    let mut buf = [0u8; 64];
    loop {
        match reader.read(&mut buf).await {
            Ok(n) => handle_output(&buf[..n]),
            Err(ReadError::Disabled) => reader.ready().await,
            Err(e) => log::warn!("hid read error: {e:?}"),
        }
    }
}

#[embassy_executor::task]
async fn writer_task(mut writer: HidWriter<'static, UsbDriver, 64>) {
    loop {
        writer.ready().await;
        // announce ourselves, as GP2040-CE (and real controllers) do
        let identify = with_protocol(|p| p.identify());
        if writer.write(&identify).await.is_err() {
            continue;
        }
        loop {
            let report = if let Ok(reply) = REPLIES.try_receive() {
                reply
            } else if with_protocol(|p| p.streaming()) {
                let state = input::get();
                with_protocol(|p| p.input_report(&state))
            } else {
                // not handshaken yet: only replies go out
                match select(REPLIES.receive(), Timer::after(Duration::from_millis(8))).await {
                    Either::First(reply) => reply,
                    Either::Second(()) => continue,
                }
            };
            // completes when the host polls the IN endpoint (every 8ms),
            // which paces the input report stream
            if let Err(e) = writer.write(&report).await {
                log::debug!("hid write failed ({e:?}), waiting for the endpoint");
                break;
            }
        }
    }
}
