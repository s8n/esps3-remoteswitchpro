//! The controller state the HTTP side writes and the USB side reports.

use core::cell::Cell;

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

/// Button bits, laid out exactly like bytes 3..=5 of a Switch input report
/// (byte 3 in bits 0-7, byte 4 in 8-15, byte 5 in 16-23).
pub mod button {
    pub const Y: u32 = 1 << 0;
    pub const X: u32 = 1 << 1;
    pub const B: u32 = 1 << 2;
    pub const A: u32 = 1 << 3;
    pub const R: u32 = 1 << 6;
    pub const ZR: u32 = 1 << 7;
    pub const MINUS: u32 = 1 << 8;
    pub const PLUS: u32 = 1 << 9;
    pub const RSTICK: u32 = 1 << 10;
    pub const LSTICK: u32 = 1 << 11;
    pub const HOME: u32 = 1 << 12;
    pub const CAPTURE: u32 = 1 << 13;
    pub const DOWN: u32 = 1 << 16;
    pub const UP: u32 = 1 << 17;
    pub const RIGHT: u32 = 1 << 18;
    pub const LEFT: u32 = 1 << 19;
    pub const L: u32 = 1 << 22;
    pub const ZL: u32 = 1 << 23;

    /// Canonical name first; the rest are accepted aliases.
    pub const NAMES: &[(u32, &[&str])] = &[
        (A, &["a"]),
        (B, &["b"]),
        (X, &["x"]),
        (Y, &["y"]),
        (L, &["l"]),
        (R, &["r"]),
        (ZL, &["zl"]),
        (ZR, &["zr"]),
        (PLUS, &["plus", "+", "start"]),
        (MINUS, &["minus", "-", "select"]),
        (HOME, &["home"]),
        (CAPTURE, &["capture", "share"]),
        (LSTICK, &["lstick", "l3", "ls"]),
        (RSTICK, &["rstick", "r3", "rs"]),
        (UP, &["up", "dup", "dpad_up"]),
        (DOWN, &["down", "ddown", "dpad_down"]),
        (LEFT, &["left", "dleft", "dpad_left"]),
        (RIGHT, &["right", "dright", "dpad_right"]),
    ];

    pub fn from_name(name: &str) -> Option<u32> {
        let name = name.trim();
        NAMES
            .iter()
            .find(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(name)))
            .map(|(bit, _)| *bit)
    }

    pub fn names(mask: u32) -> impl Iterator<Item = &'static str> {
        NAMES
            .iter()
            .filter(move |(bit, _)| mask & bit != 0)
            .map(|(_, names)| names[0])
    }
}

/// Stick position, -1.0..=1.0 on both axes, y up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stick {
    pub x: f32,
    pub y: f32,
}

impl Stick {
    pub const CENTER: Stick = Stick { x: 0.0, y: 0.0 };

    pub fn clamped(x: f32, y: f32) -> Stick {
        let c = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
        Stick { x: c(x), y: c(y) }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    pub buttons: u32,
    pub left: Stick,
    pub right: Stick,
}

static STATE: Mutex<Cell<State>> = Mutex::new(Cell::new(State {
    buttons: 0,
    left: Stick::CENTER,
    right: Stick::CENTER,
}));

/// Raised when a button goes down; the USB task uses it to wake a
/// suspended (sleeping) console.
pub static PRESSED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn get() -> State {
    critical_section::with(|cs| STATE.borrow(cs).get())
}

pub fn update(f: impl FnOnce(&mut State)) {
    let pressed = critical_section::with(|cs| {
        let cell = STATE.borrow(cs);
        let mut s = cell.get();
        let before = s.buttons;
        f(&mut s);
        cell.set(s);
        s.buttons & !before != 0
    });
    if pressed {
        PRESSED.signal(());
    }
}

pub fn set(state: State) {
    update(|s| *s = state);
}
