//! Wifi station bring-up and reconnection.

use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Rng;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Config as WifiConfig, ControllerConfig, Interface, WifiController};
use static_cell::StaticCell;

use crate::config;

static IP: critical_section::Mutex<core::cell::Cell<Option<embassy_net::Ipv4Address>>> =
    critical_section::Mutex::new(core::cell::Cell::new(None));

/// The current DHCP address, if there is one.
pub fn ip() -> Option<embassy_net::Ipv4Address> {
    critical_section::with(|cs| IP.borrow(cs).get())
}

pub fn init(spawner: embassy_executor::Spawner, wifi: WIFI<'static>) -> Option<Stack<'static>> {
    let config = config::get();

    let station_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(config.wifi.ssid.as_str())
            .with_password(config.wifi.password.clone()),
    );

    let (controller, interfaces) = match esp_radio::wifi::new(
        wifi,
        ControllerConfig::default().with_initial_config(station_config),
    ) {
        Ok(parts) => parts,
        Err(e) => {
            log::error!("failed to initialize wifi: {e:?}");
            return None;
        }
    };

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let mut dhcp = embassy_net::DhcpConfig::default();
    dhcp.hostname = config.hostname.as_str().try_into().ok();
    if dhcp.hostname.is_none() {
        log::warn!("hostname too long for DHCP (max 32), not sending one");
    }

    static STACK_RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(dhcp),
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(report_ip_task(stack).unwrap());
    Some(stack)
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    let mut failures = 0u32;
    loop {
        if controller.is_connected() {
            let _ = controller.wait_for_disconnect_async().await;
            log::info!("wifi disconnected");
            continue;
        }
        match controller.connect_async().await {
            Ok(info) => {
                failures = 0;
                log::info!("wifi connected: {info:?}");
            }
            Err(e) => {
                log::warn!("wifi connect failed: {e:?}");
                // every few failures, list what is in range to help debugging
                failures += 1;
                if failures % 6 == 1 {
                    match controller
                        .scan_async(&esp_radio::wifi::scan::ScanConfig::default())
                        .await
                    {
                        Ok(aps) => {
                            log::info!("{} networks in range:", aps.len());
                            for ap in aps {
                                log::info!(
                                    "  {:?} ch {} {} dBm",
                                    ap.ssid.as_str(),
                                    ap.channel,
                                    ap.signal_strength
                                );
                            }
                        }
                        Err(e) => log::warn!("wifi scan failed: {e:?}"),
                    }
                }
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn report_ip_task(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        if let Some(cfg) = stack.config_v4() {
            log::info!("got address {}", cfg.address);
            critical_section::with(|cs| IP.borrow(cs).set(Some(cfg.address.address())));
        }
        stack.wait_config_down().await;
        critical_section::with(|cs| IP.borrow(cs).set(None));
    }
}
