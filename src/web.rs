//! The HTTP API and the control page.
//!
//! See README.md for the endpoints. Button names are listed in
//! `input::button::NAMES`.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use picoserve::extract::FromRequestParts;
use picoserve::request::RequestParts;
use picoserve::response::fs::File;
use picoserve::response::{Content, StatusCode};
use picoserve::routing::{PathRouter, get, get_service, post};
use picoserve::{Router, Timeouts};
use serde::Deserialize;

use crate::input::{self, Stick, button};
use crate::{config, logger, system, usb};

const WEB_TASK_POOL_SIZE: usize = 3;
/// Longest single press, stick hold or sequence step.
const MAX_STEP_MS: u32 = 60_000;
/// Cap on steps × repeats in one sequence.
const MAX_SEQUENCE_STEPS: usize = 10_000;
/// A press shorter than a couple of 8ms input reports can be missed.
const MIN_PRESS_MS: u32 = 20;
const DEFAULT_PRESS_MS: u32 = 100;

const CONTROL_PAGE: &str = include_str!("control.html");

/// A text body with an explicit content type.
struct TypedText(&'static str, String);

impl Content for TypedText {
    fn content_type(&self) -> &'static str {
        self.0
    }

    fn content_length(&self) -> usize {
        self.1.len()
    }

    async fn write_content<W: picoserve::io::Write>(self, mut writer: W) -> Result<(), W::Error> {
        writer.write_all(self.1.as_bytes()).await
    }
}

type Response = (StatusCode, TypedText);

fn text(status: StatusCode, message: impl Into<String>) -> Response {
    let mut message = message.into();
    message.push('\n');
    (status, TypedText("text/plain; charset=utf-8", message))
}

fn ok() -> Response {
    text(StatusCode::OK, "ok")
}

fn bad_request(message: impl Into<String>) -> Response {
    text(StatusCode::BAD_REQUEST, message)
}

fn json(value: &impl serde::Serialize) -> Response {
    match serde_json::to_string(value) {
        Ok(s) => (StatusCode::OK, TypedText("application/json", s)),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "serialization failed"),
    }
}

/// Extractor that enforces the configured API token, if any.
struct Auth;

impl<'r, S> FromRequestParts<'r, S> for Auth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        _state: &'r S,
        parts: &RequestParts<'r>,
    ) -> Result<Self, Self::Rejection> {
        let token = config::get().token.as_str();
        if token.is_empty() {
            return Ok(Auth);
        }
        let header_ok = parts
            .headers()
            .get("Authorization")
            .and_then(|v| v.as_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|v| v.trim() == token);
        let query_ok = parts.query().is_some_and(|q| {
            q.0.split('&')
                .any(|kv| kv.strip_prefix("token=").is_some_and(|v| v == token))
        });
        if header_ok || query_ok {
            Ok(Auth)
        } else {
            Err((StatusCode::UNAUTHORIZED, "missing or wrong token\n"))
        }
    }
}

/// Either `["a", "b"]` or `"a+b"` / `"a,b"` / `"a b"`.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Buttons {
    #[default]
    None,
    List(Vec<String>),
    One(String),
}

impl Buttons {
    fn mask(&self) -> Result<u32, String> {
        let mut mask = 0;
        let mut add = |name: &str| -> Result<(), String> {
            if name.is_empty() {
                return Ok(());
            }
            mask |= button::from_name(name).ok_or_else(|| alloc::format!("unknown button {name:?}"))?;
            Ok(())
        };
        match self {
            Buttons::None => {}
            Buttons::List(names) => {
                for name in names {
                    add(name.trim())?;
                }
            }
            Buttons::One(names) => {
                // a lone "+" or "-" is a button name, not a separator
                if button::from_name(names).is_some() {
                    add(names.trim())?;
                } else {
                    for name in names.split(['+', ',', ' ']) {
                        add(name)?;
                    }
                }
            }
        }
        Ok(mask)
    }
}

fn clamp_ms(ms: u32) -> u32 {
    ms.min(MAX_STEP_MS)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ButtonsRequest {
    buttons: Buttons,
    ms: Option<u32>,
}

/// JSON, or a bare button list as the whole body (`curl -d a+b`).
fn parse_buttons_request(body: &str) -> Result<ButtonsRequest, Response> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(ButtonsRequest::default());
    }
    if body.starts_with('{') {
        serde_json::from_str(body).map_err(|e| bad_request(alloc::format!("invalid json: {e}")))
    } else {
        Ok(ButtonsRequest {
            buttons: Buttons::One(body.into()),
            ms: None,
        })
    }
}

/// `POST /api/press`: hold the buttons for `ms` (default 100), then release.
async fn press(_: Auth, body: String) -> Response {
    let req = match parse_buttons_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let mask = match req.buttons.mask() {
        Ok(m) => m,
        Err(e) => return bad_request(e),
    };
    if mask == 0 {
        return bad_request("no buttons given");
    }
    let ms = clamp_ms(req.ms.unwrap_or(DEFAULT_PRESS_MS)).max(MIN_PRESS_MS);
    input::update(|s| s.buttons |= mask);
    Timer::after(Duration::from_millis(ms.into())).await;
    input::update(|s| s.buttons &= !mask);
    ok()
}

/// `POST /api/hold`: press and keep holding until released.
async fn hold(_: Auth, body: String) -> Response {
    let req = match parse_buttons_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match req.buttons.mask() {
        Ok(0) => bad_request("no buttons given"),
        Ok(mask) => {
            input::update(|s| s.buttons |= mask);
            ok()
        }
        Err(e) => bad_request(e),
    }
}

/// `POST /api/release`: release the given buttons, or all of them.
async fn release(_: Auth, body: String) -> Response {
    let req = match parse_buttons_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match req.buttons.mask() {
        Ok(0) => input::update(|s| s.buttons = 0),
        Ok(mask) => input::update(|s| s.buttons &= !mask),
        Err(e) => return bad_request(e),
    }
    ok()
}

#[derive(Deserialize)]
struct StickRequest {
    stick: String,
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    ms: Option<u32>,
}

fn which_stick(name: &str) -> Option<bool> {
    match name {
        "left" | "l" | "ls" => Some(true),
        "right" | "r" | "rs" => Some(false),
        _ => None,
    }
}

/// `POST /api/stick`: move a stick (x, y in -1..1, y up); with `ms`, return
/// it to the centre after that long.
async fn stick(_: Auth, body: String) -> Response {
    let req: StickRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return bad_request(alloc::format!("invalid json: {e}")),
    };
    let Some(left) = which_stick(&req.stick) else {
        return bad_request("stick must be \"left\" or \"right\"");
    };
    let pos = Stick::clamped(req.x, req.y);
    let set = |pos: Stick| {
        input::update(|s| {
            if left {
                s.left = pos;
            } else {
                s.right = pos;
            }
        })
    };
    set(pos);
    if let Some(ms) = req.ms {
        Timer::after(Duration::from_millis(clamp_ms(ms).into())).await;
        set(Stick::CENTER);
    }
    ok()
}

#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    buttons: Buttons,
    left: Option<[f32; 2]>,
    right: Option<[f32; 2]>,
    #[serde(default)]
    ms: u32,
}

#[derive(Deserialize)]
struct SequenceRequest {
    steps: Vec<Step>,
    #[serde(default)]
    repeat: Option<u32>,
}

/// `POST /api/sequence`: play steps, each setting the whole controller state
/// (unlisted buttons released, unlisted sticks centred) for `ms`. Ends in
/// the neutral state. Answers once the sequence has finished.
async fn sequence(_: Auth, body: String) -> Response {
    let req: SequenceRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return bad_request(alloc::format!("invalid json: {e}")),
    };
    let repeat = req.repeat.unwrap_or(1).max(1) as usize;
    if req.steps.len().saturating_mul(repeat) > MAX_SEQUENCE_STEPS {
        return bad_request(alloc::format!("more than {MAX_SEQUENCE_STEPS} steps in total"));
    }
    let mut states = Vec::with_capacity(req.steps.len());
    for (i, step) in req.steps.iter().enumerate() {
        let buttons = match step.buttons.mask() {
            Ok(m) => m,
            Err(e) => return bad_request(alloc::format!("step {i}: {e}")),
        };
        let stick = |v: Option<[f32; 2]>| v.map_or(Stick::CENTER, |[x, y]| Stick::clamped(x, y));
        states.push((
            input::State {
                buttons,
                left: stick(step.left),
                right: stick(step.right),
            },
            clamp_ms(step.ms),
        ));
    }
    for _ in 0..repeat {
        for (state, ms) in &states {
            input::set(*state);
            Timer::after(Duration::from_millis((*ms).into())).await;
        }
    }
    input::set(input::State::default());
    ok()
}

/// `POST /api/neutral`: release everything, centre both sticks.
async fn neutral(_: Auth) -> Response {
    input::set(input::State::default());
    ok()
}

#[derive(serde::Serialize)]
struct InputStatus {
    buttons: Vec<&'static str>,
    left: [f32; 2],
    right: [f32; 2],
}

#[derive(serde::Serialize)]
struct Status {
    usb: usb::Status,
    input: InputStatus,
    ip: Option<String>,
    uptime_s: u64,
    free_heap: usize,
    last_reset: Option<&'static system::LastReset>,
}

async fn status(_: Auth) -> Response {
    let s = input::get();
    let ip = crate::net::ip().map(|a| alloc::format!("{a}"));
    json(&Status {
        usb: usb::status(),
        input: InputStatus {
            buttons: button::names(s.buttons).collect(),
            left: [s.left.x, s.left.y],
            right: [s.right.x, s.right.y],
        },
        ip,
        uptime_s: embassy_time::Instant::now().as_secs(),
        free_heap: esp_alloc::HEAP.free(),
        last_reset: system::last_reset(),
    })
}

async fn logs(_: Auth) -> Response {
    (StatusCode::OK, TypedText("text/plain; charset=utf-8", logger::snapshot()))
}

async fn reboot(_: Auth) -> Response {
    system::request_reboot(system::RebootKind::Normal);
    text(StatusCode::OK, "rebooting")
}

async fn bootloader(_: Auth) -> Response {
    system::request_reboot(system::RebootKind::Bootloader);
    text(
        StatusCode::OK,
        "rebooting into the ROM bootloader; flash over USB-Serial-JTAG now",
    )
}

fn make_app() -> Router<impl PathRouter> {
    Router::new()
        .route("/", get_service(File::html(CONTROL_PAGE)))
        .route("/api/status", get(status))
        .route("/api/logs", get(logs))
        .route("/api/press", post(press))
        .route("/api/hold", post(hold))
        .route("/api/release", post(release))
        .route("/api/stick", post(stick))
        .route("/api/sequence", post(sequence))
        .route("/api/neutral", post(neutral))
        .route("/api/reboot", post(reboot))
        .route("/api/bootloader", post(bootloader))
}

pub fn spawn(spawner: embassy_executor::Spawner, stack: Stack<'static>) {
    for task_id in 0..WEB_TASK_POOL_SIZE {
        spawner.spawn(web_task(task_id, stack).unwrap());
    }
}

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(task_id: usize, stack: Stack<'static>) {
    let server_config = picoserve::Config::new(Timeouts {
        start_read_request: Duration::from_secs(10),
        persistent_start_read_request: Duration::from_secs(5),
        read_request: Duration::from_secs(10),
        write: Duration::from_secs(5),
    })
    .keep_connection_alive();

    let app = make_app();
    let mut tcp_rx_buffer = [0; 1536];
    let mut tcp_tx_buffer = [0; 1536];
    let mut http_buffer = [0; 4096];

    picoserve::Server::new(&app, &server_config, &mut http_buffer)
        .listen_and_serve(
            task_id,
            stack,
            config::get().http_port,
            &mut tcp_rx_buffer,
            &mut tcp_tx_buffer,
        )
        .await
        .into_never()
}
