// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS activity detection and synthetic input: the phone as touchpad,
//! keyboard and presentation remote, Deck key presses and typing, and touches
//! from phone mirroring. Events go in through CoreGraphics, which needs the
//! user to allow NectarLink under System Settings > Privacy & Security >
//! Accessibility; without it macOS drops them silently, so [`is_available`]
//! tells the phone not to offer input until it is on.
//!
//! Never log what is typed or pressed.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use nectarlink_core::{ButtonAction, KeyMod, MouseButton, PowerAction, SlideAction, TouchAction};

mod cg;

/// Whether this app may post input events now (the Accessibility switch).
pub fn is_available() -> bool {
    cg::post_access()
}

/// Asks macOS for the permission to post input events. It opens the system
/// prompt that leads to the Accessibility pane the first time, and does nothing
/// after the user has answered. Returns whether the permission is on.
pub fn request_access() -> bool {
    cg::request_post_access()
}

/// Calls `granted` once, from a background thread, when the permission turns on.
/// It looks every two seconds for two minutes: macOS tells no one when the
/// user flips the switch.
pub fn watch_access(granted: fn()) {
    if is_available() {
        return;
    }
    let spawned = std::thread::Builder::new().name("input-access-watch".into()).spawn(move || {
        for _ in 0..60 {
            std::thread::sleep(Duration::from_secs(2));
            if is_available() {
                granted();
                return;
            }
        }
    });
    drop(spawned);
}

/// Wheel units for one notch of the protocol's scroll. The events are in
/// pixels, which scrolls smoothly in every app.
const PIXELS_PER_NOTCH: f32 = 40.0;
/// Two presses of a button this close in time and place are a double click.
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 4.0;
/// How long the app gets to read one typed character before the next, and a
/// key press: the values that the Windows build needed (`win/input.rs`).
const BETWEEN_CHARACTERS: Duration = Duration::from_millis(16);
const AFTER_KEY: Duration = Duration::from_millis(32);

#[derive(Default)]
struct State {
    held: [bool; 3],
    move_rem: (f32, f32),
    scroll_rem: (f32, f32),
    last_click: Option<LastClick>,
    next_keystroke: Option<Instant>,
}

struct LastClick {
    at: Instant,
    button: usize,
    point: cg::Point,
    count: i64,
}

static STATE: Mutex<State> = Mutex::new(State {
    held: [false; 3],
    move_rem: (0.0, 0.0),
    scroll_rem: (0.0, 0.0),
    last_click: None,
    next_keystroke: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

/// The index of the first button that is held, for the "dragged" event type.
fn held_button(held: &[bool; 3]) -> Option<usize> {
    held.iter().position(|&h| h)
}

/// Moves the cursor by `(dx, dy)` points, keeping fractions for the next move.
pub fn move_relative(dx: f32, dy: f32) {
    if !dx.is_finite() || !dy.is_finite() {
        return;
    }
    let (ix, iy, held) = {
        let mut s = state();
        let total_x = s.move_rem.0 + dx;
        let total_y = s.move_rem.1 + dy;
        let ix = total_x.round();
        let iy = total_y.round();
        s.move_rem = (total_x - ix, total_y - iy);
        (ix as i64, iy as i64, held_button(&s.held))
    };
    if ix == 0 && iy == 0 {
        return;
    }
    let Some(at) = cg::cursor() else { return };
    let to = cg::clamp_to_displays(cg::Point { x: at.x + ix as f64, y: at.y + iy as f64 });
    cg::post_mouse_move(to, held, (ix, iy));
}

/// Moves the cursor to normalized coordinates `(x, y)` on the main display
/// (`0.0..=1.0`).
#[allow(dead_code)]
pub fn move_absolute(x: f32, y: f32) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let held = held_button(&state().held);
    cg::post_mouse_move(on_main_display(x, y), held, (0, 0));
}

/// A touch from reverse PC mirroring: the cursor goes to `(x, y)` and the left
/// button follows the finger.
#[allow(dead_code)]
pub fn mirror_touch(action: TouchAction, x: f32, y: f32) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let point = on_main_display(x, y);
    match action {
        TouchAction::Down => {
            cg::post_mouse_move(point, None, (0, 0));
            state().held[0] = true;
            cg::post_mouse_button(0, true, point, 1);
        }
        TouchAction::Move => {
            let held = held_button(&state().held);
            cg::post_mouse_move(point, held, (0, 0));
        }
        TouchAction::Up => {
            state().held[0] = false;
            cg::post_mouse_button(0, false, point, 1);
        }
    }
}

fn on_main_display(x: f32, y: f32) -> cg::Point {
    let bounds = cg::main_display_bounds();
    normalized_point(bounds, x, y)
}

/// The point at `(x, y)`, each in `0.0..=1.0`, inside `bounds`.
fn normalized_point(bounds: cg::Rect, x: f32, y: f32) -> cg::Point {
    let fx = f64::from(x.clamp(0.0, 1.0));
    let fy = f64::from(y.clamp(0.0, 1.0));
    // The last pixel is width - 1: a point on the far edge belongs to no display.
    cg::Point {
        x: bounds.x + fx * (bounds.width - 1.0).max(0.0),
        y: bounds.y + fy * (bounds.height - 1.0).max(0.0),
    }
}

/// Presses, releases or clicks a mouse button where the cursor is.
pub fn mouse_button(button: MouseButton, action: ButtonAction) {
    let index = button_index(button);
    let Some(at) = cg::cursor() else { return };
    let press = |s: &mut State| {
        s.held[index] = true;
        click_count(s, index, at)
    };
    match action {
        ButtonAction::Down => {
            let count = press(&mut state());
            cg::post_mouse_button(index, true, at, count);
        }
        ButtonAction::Up => {
            let count = {
                let mut s = state();
                s.held[index] = false;
                s.last_click.as_ref().map_or(1, |c| c.count)
            };
            cg::post_mouse_button(index, false, at, count);
        }
        ButtonAction::Click => {
            let count = press(&mut state());
            cg::post_mouse_button(index, true, at, count);
            state().held[index] = false;
            cg::post_mouse_button(index, false, at, count);
        }
    }
}

/// The click count of a press at `point`: it grows while presses of the same
/// button come quickly and close together, which is how a double click works.
fn click_count(s: &mut State, button: usize, point: cg::Point) -> i64 {
    let now = Instant::now();
    let count = match &s.last_click {
        Some(last)
            if last.button == button
                && now.duration_since(last.at) <= DOUBLE_CLICK_TIME
                && (last.point.x - point.x).abs() <= DOUBLE_CLICK_DISTANCE
                && (last.point.y - point.y).abs() <= DOUBLE_CLICK_DISTANCE =>
        {
            last.count + 1
        }
        _ => 1,
    };
    s.last_click = Some(LastClick { at: now, button, point, count });
    count
}

/// Releases any mouse buttons still held down by remote input.
pub fn release_held_buttons() {
    let held = {
        let mut s = state();
        let held = s.held;
        s.held = [false; 3];
        s.move_rem = (0.0, 0.0);
        s.scroll_rem = (0.0, 0.0);
        held
    };
    let Some(at) = cg::cursor() else { return };
    for (index, _) in held.iter().enumerate().filter(|(_, h)| **h) {
        cg::post_mouse_button(index, false, at, 1);
    }
}

/// Scrolls smoothly (`dy` > 0 scrolls down, `dx` > 0 scrolls right, in notches).
pub fn scroll(dx: f32, dy: f32) {
    if !dx.is_finite() || !dy.is_finite() {
        return;
    }
    let (px, py) = {
        let mut s = state();
        // macOS wheel values are positive for up and for left.
        let total_x = s.scroll_rem.0 - dx * PIXELS_PER_NOTCH;
        let total_y = s.scroll_rem.1 - dy * PIXELS_PER_NOTCH;
        let px = total_x.round();
        let py = total_y.round();
        s.scroll_rem = (total_x - px, total_y - py);
        (px as i32, py as i32)
    };
    if px != 0 || py != 0 {
        cg::post_scroll(py, px);
    }
}

/// Waits until the app has had its time since the last character or key press,
/// and books `gap` for this one. Voice typing rewrites a phrase with Backspaces
/// and then the new text, and a letter sent right after a Backspace could get
/// lost.
fn wait_for_last_keystroke(gap: Duration) {
    let left = {
        let mut s = state();
        let left = s.next_keystroke.and_then(|at| at.checked_duration_since(Instant::now()));
        s.next_keystroke = Some(Instant::now() + left.unwrap_or_default() + gap);
        left
    };
    if let Some(left) = left {
        std::thread::sleep(left);
    }
}

/// Types arbitrary Unicode text, one character at a time.
pub fn type_text(text: &str) {
    for ch in text.chars() {
        match ch {
            '\r' => continue,
            '\n' => {
                wait_for_last_keystroke(AFTER_KEY);
                cg::post_key(KEY_RETURN, true, 0, None);
                cg::post_key(KEY_RETURN, false, 0, None);
            }
            '\t' => {
                wait_for_last_keystroke(AFTER_KEY);
                cg::post_key(KEY_TAB, true, 0, None);
                cg::post_key(KEY_TAB, false, 0, None);
            }
            _ => {
                let mut buf = [0u16; 2];
                let units = ch.encode_utf16(&mut buf);
                wait_for_last_keystroke(BETWEEN_CHARACTERS);
                cg::post_key(0, true, 0, Some(units));
                cg::post_key(0, false, 0, Some(units));
            }
        }
    }
}

// Virtual key codes (Carbon `kVK_*`, by position on an ANSI keyboard).
const KEY_RETURN: u16 = 0x24;
const KEY_TAB: u16 = 0x30;

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

/// The key code and the flag of a modifier. `Win` is Command and `Alt` is
/// Option, so a phone's shortcuts land on the Mac's own.
fn modifier(m: KeyMod) -> (u16, u64) {
    match m {
        KeyMod::Shift => (0x38, FLAG_SHIFT),
        KeyMod::Ctrl => (0x3B, FLAG_CONTROL),
        KeyMod::Alt => (0x3A, FLAG_OPTION),
        KeyMod::Win => (0x37, FLAG_COMMAND),
    }
}

/// Resolves a protocol key or shortcut name into its key code and the
/// modifiers that a shortcut alias adds. Media keys and `volume_*` are not
/// here: macOS takes those as system events, not key codes.
fn resolve_key(key: &str) -> Option<(u16, &'static [KeyMod])> {
    const NONE: &[KeyMod] = &[];
    const CMD: &[KeyMod] = &[KeyMod::Win];
    const CMD_SHIFT: &[KeyMod] = &[KeyMod::Win, KeyMod::Shift];
    const CTRL: &[KeyMod] = &[KeyMod::Ctrl];
    Some(match key {
        "copy" => (0x08, CMD),
        "paste" => (0x09, CMD),
        "cut" => (0x07, CMD),
        "undo" => (0x06, CMD),
        "redo" => (0x06, CMD_SHIFT),
        "select_all" => (0x00, CMD),
        // Mission Control, with its default shortcut.
        "task_view" => (0x7E, CTRL),
        "enter" => (KEY_RETURN, NONE),
        "backspace" => (0x33, NONE),
        "tab" => (KEY_TAB, NONE),
        "escape" => (0x35, NONE),
        "space" => (0x31, NONE),
        "delete" => (0x75, NONE),
        "left" => (0x7B, NONE),
        "right" => (0x7C, NONE),
        "down" => (0x7D, NONE),
        "up" => (0x7E, NONE),
        "home" => (0x73, NONE),
        "end" => (0x77, NONE),
        "page_up" => (0x74, NONE),
        "page_down" => (0x79, NONE),
        "f1" => (0x7A, NONE),
        "f2" => (0x78, NONE),
        "f3" => (0x63, NONE),
        "f4" => (0x76, NONE),
        "f5" => (0x60, NONE),
        "f6" => (0x61, NONE),
        "f7" => (0x62, NONE),
        "f8" => (0x64, NONE),
        "f9" => (0x65, NONE),
        "f10" => (0x6D, NONE),
        "f11" => (0x67, NONE),
        "f12" => (0x6F, NONE),
        s if s.len() == 1 => (letter_or_digit(s.as_bytes()[0])?, NONE),
        _ => return None,
    })
}

/// The key code of an ASCII lowercase letter or digit, by its place on the keyboard.
fn letter_or_digit(b: u8) -> Option<u16> {
    Some(match b {
        b'a' => 0x00,
        b's' => 0x01,
        b'd' => 0x02,
        b'f' => 0x03,
        b'h' => 0x04,
        b'g' => 0x05,
        b'z' => 0x06,
        b'x' => 0x07,
        b'c' => 0x08,
        b'v' => 0x09,
        b'b' => 0x0B,
        b'q' => 0x0C,
        b'w' => 0x0D,
        b'e' => 0x0E,
        b'r' => 0x0F,
        b'y' => 0x10,
        b't' => 0x11,
        b'1' => 0x12,
        b'2' => 0x13,
        b'3' => 0x14,
        b'4' => 0x15,
        b'6' => 0x16,
        b'5' => 0x17,
        b'9' => 0x19,
        b'7' => 0x1A,
        b'8' => 0x1C,
        b'0' => 0x1D,
        b'o' => 0x1F,
        b'u' => 0x20,
        b'i' => 0x22,
        b'p' => 0x23,
        b'l' => 0x25,
        b'j' => 0x26,
        b'k' => 0x28,
        b'n' => 0x2D,
        b'm' => 0x2E,
        _ => return None,
    })
}

/// Presses a named key or shortcut with optional modifiers.
pub fn press_key(key: &str, mods: &[KeyMod]) {
    if key == "lock" {
        let _ = crate::links::power(PowerAction::Lock);
        return;
    }
    let Some((code, preset)) = resolve_key(key) else { return };
    let mut all: Vec<KeyMod> = Vec::with_capacity(4);
    for &m in preset.iter().chain(mods) {
        if !all.contains(&m) {
            all.push(m);
        }
    }
    wait_for_last_keystroke(AFTER_KEY);
    // Modifiers go down one by one, the key goes down and up with all of them
    // set, and the modifiers come up in the opposite order.
    let mut flags = 0;
    for &m in &all {
        let (mod_code, flag) = modifier(m);
        flags |= flag;
        cg::post_key(mod_code, true, flags, None);
    }
    cg::post_key(code, true, flags, None);
    cg::post_key(code, false, flags, None);
    for &m in all.iter().rev() {
        let (mod_code, flag) = modifier(m);
        flags &= !flag;
        cg::post_key(mod_code, false, flags, None);
    }
}

/// Executes a presentation slide action.
pub fn slide(action: SlideAction) {
    let code = match action {
        SlideAction::Next => 0x79,     // Page Down
        SlideAction::Previous => 0x74, // Page Up
        SlideAction::Start => 0x60,    // F5
        SlideAction::Stop => 0x35,     // Escape
        SlideAction::Black => 0x0B,    // B
    };
    cg::post_key(code, true, 0, None);
    cg::post_key(code, false, 0, None);
}

/// Whether the session received keyboard, mouse or trackpad input recently.
pub fn is_user_active_within_ms(max_idle_ms: u32) -> bool {
    let idle_secs = cg::seconds_since_last_event();
    active_within_ms(idle_secs, max_idle_ms)
}

fn active_within_ms(idle_secs: f64, max_idle_ms: u32) -> bool {
    !idle_secs.is_finite() || idle_secs < 0.0 || idle_secs * 1000.0 <= f64::from(max_idle_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_window_includes_its_boundary() {
        assert!(active_within_ms(0.0, 0));
        assert!(active_within_ms(29.999, 30_000));
        assert!(active_within_ms(30.0, 30_000));
        assert!(!active_within_ms(30.001, 30_000));
    }

    #[test]
    fn invalid_idle_readings_count_as_activity() {
        for idle in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(active_within_ms(idle, 0));
        }
    }

    #[test]
    fn every_key_the_protocol_names_has_a_code() {
        use nectarlink_core::remote_keys;
        for name in remote_keys::NAMED.iter().filter(|n| **n != remote_keys::WIN) {
            assert!(resolve_key(name).is_some(), "{name}");
        }
        for name in remote_keys::SHORTCUTS.iter().filter(|n| **n != remote_keys::LOCK) {
            assert!(resolve_key(name).is_some(), "{name}");
        }
        for b in (b'a'..=b'z').chain(b'0'..=b'9') {
            assert!(resolve_key(&char::from(b).to_string()).is_some(), "{}", char::from(b));
        }
        assert_eq!(resolve_key("not_a_key"), None);
        assert_eq!(resolve_key("A"), None);
    }

    #[test]
    fn key_codes_do_not_repeat() {
        use std::collections::HashSet;
        let mut codes = HashSet::new();
        let letters_and_digits = (b'a'..=b'z').chain(b'0'..=b'9').map(|b| char::from(b).to_string());
        for name in nectarlink_core::remote_keys::NAMED
            .iter()
            .map(|n| (*n).to_string())
            .filter(|n| n != "win")
            .chain(letters_and_digits)
        {
            let (code, _) = resolve_key(&name).unwrap();
            assert!(codes.insert(code), "{name} repeats a key code");
        }
    }

    #[test]
    fn shortcuts_use_command_not_control() {
        assert_eq!(resolve_key("copy"), Some((0x08, &[KeyMod::Win][..])));
        assert_eq!(resolve_key("redo"), Some((0x06, &[KeyMod::Win, KeyMod::Shift][..])));
        assert_eq!(modifier(KeyMod::Win).1, FLAG_COMMAND);
        assert_eq!(modifier(KeyMod::Alt).1, FLAG_OPTION);
    }

    #[test]
    fn a_point_on_the_display_stays_inside_it() {
        let bounds = cg::Rect { x: -1920.0, y: 0.0, width: 1920.0, height: 1080.0 };
        let top_left = normalized_point(bounds, 0.0, 0.0);
        assert_eq!((top_left.x, top_left.y), (-1920.0, 0.0));
        let bottom_right = normalized_point(bounds, 1.0, 1.0);
        assert_eq!((bottom_right.x, bottom_right.y), (-1.0, 1079.0));
        let outside = normalized_point(bounds, 9.0, -9.0);
        assert_eq!((outside.x, outside.y), (-1.0, 0.0));
    }

    #[test]
    fn clicks_in_a_row_count_up_until_they_stop() {
        let mut s = State::default();
        let here = cg::Point { x: 100.0, y: 100.0 };
        assert_eq!(click_count(&mut s, 0, here), 1);
        assert_eq!(click_count(&mut s, 0, here), 2);
        assert_eq!(click_count(&mut s, 0, cg::Point { x: 101.0, y: 99.0 }), 3);
        // Another button, or another place, starts over.
        assert_eq!(click_count(&mut s, 1, here), 1);
        assert_eq!(click_count(&mut s, 1, cg::Point { x: 300.0, y: 100.0 }), 1);
        // So does waiting.
        s.last_click.as_mut().unwrap().at = Instant::now() - DOUBLE_CLICK_TIME * 2;
        assert_eq!(click_count(&mut s, 1, cg::Point { x: 300.0, y: 100.0 }), 1);
    }

    /// Moves the real cursor, so it runs only by hand, with the Accessibility switch on:
    /// `cargo test -p nectarlink-desktop -- --ignored cursor_moves`
    #[test]
    #[ignore = "moves the real cursor and needs the Accessibility switch"]
    fn cursor_moves_and_comes_back() {
        assert!(is_available(), "turn NectarLink (or the terminal) on under Accessibility");
        let start = cg::cursor().unwrap();
        move_relative(-30.0, -20.0);
        std::thread::sleep(Duration::from_millis(150));
        let moved = cg::cursor().unwrap();
        move_relative(30.0, 20.0);
        std::thread::sleep(Duration::from_millis(150));
        let back = cg::cursor().unwrap();
        assert_eq!((moved.x - start.x, moved.y - start.y), (-30.0, -20.0));
        assert_eq!((back.x, back.y), (start.x, start.y));
    }

    #[test]
    fn the_first_held_button_makes_the_drag() {
        assert_eq!(held_button(&[false, false, false]), None);
        assert_eq!(held_button(&[false, true, true]), Some(1));
    }
}
