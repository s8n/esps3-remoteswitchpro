//! Build-time configuration: config.json (or config.json.template) is baked
//! into the firmware by build.rs and parsed once at boot.

use alloc::string::String;
use embassy_sync::once_lock::OnceLock;
use serde::Deserialize;

const CONFIG_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/config.json"));

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub wifi: WifiConfig,
    /// Sent as the DHCP hostname, so routers that register DHCP names in DNS
    /// (a FRITZ!Box does) make the device reachable by name.
    pub hostname: String,
    pub http_port: u16,
    /// If non-empty, every /api request must carry it, either as
    /// `Authorization: Bearer <token>` or as `?token=<token>`.
    pub token: String,
    pub colors: Colors,
    /// error, warn, info, debug or trace
    pub log_level: String,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct WifiConfig {
    pub ssid: String,
    pub password: String,
}

/// The controller colours the Switch shows in its menus, as `#RRGGBB`.
#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct Colors {
    pub body: String,
    pub buttons: String,
    pub left_grip: String,
    pub right_grip: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            body: "#1B1B1D".into(),
            buttons: "#FFFFFF".into(),
            left_grip: "#EC008C".into(),
            right_grip: "#EC008C".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::default(),
            hostname: "remoteswitchpro".into(),
            http_port: 80,
            token: String::new(),
            colors: Colors::default(),
            log_level: "info".into(),
        }
    }
}

/// `#RRGGBB` (or `RRGGBB`) to bytes; None if malformed.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn init() {
    let config = match serde_json::from_str(CONFIG_JSON) {
        Ok(c) => c,
        Err(e) => {
            log::error!("baked-in config does not parse ({e}), using defaults");
            Config::default()
        }
    };
    let _ = CONFIG.init(config);
}

pub fn get() -> &'static Config {
    CONFIG.try_get().expect("config initialized at boot")
}
