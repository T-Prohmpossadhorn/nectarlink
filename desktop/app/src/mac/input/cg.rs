// SPDX-License-Identifier: GPL-3.0-or-later
//! The CoreGraphics calls behind `mac/input.rs`, with the `unsafe` kept here.
//! Every event is made, posted and released inside one function, and none
//! of the functions keeps a pointer.

#![allow(unsafe_code)]

use std::{ffi::c_void, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Size {
    width: f64,
    height: f64,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RawRect {
    origin: Point,
    size: Size,
}

/// A rectangle in global display coordinates (points, origin at the top left
/// of the main display).
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<RawRect> for Rect {
    fn from(r: RawRect) -> Self {
        Rect { x: r.origin.x, y: r.origin.y, width: r.size.width, height: r.size.height }
    }
}

type EventRef = *mut c_void;
type SourceRef = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    safe fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    safe fn CGPreflightPostEventAccess() -> bool;
    safe fn CGRequestPostEventAccess() -> bool;
    safe fn CGMainDisplayID() -> u32;
    safe fn CGDisplayBounds(display: u32) -> RawRect;
    fn CGGetActiveDisplayList(max_displays: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGEventSourceCreate(state_id: i32) -> SourceRef;
    fn CGEventCreate(source: SourceRef) -> EventRef;
    fn CGEventGetLocation(event: EventRef) -> Point;
    fn CGEventCreateMouseEvent(source: SourceRef, kind: u32, position: Point, button: u32) -> EventRef;
    fn CGEventCreateKeyboardEvent(source: SourceRef, key: u16, down: bool) -> EventRef;
    fn CGEventKeyboardSetUnicodeString(event: EventRef, length: usize, units: *const u16);
    fn CGEventCreateScrollWheelEvent2(
        source: SourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> EventRef;
    fn CGEventSetFlags(event: EventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: EventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: EventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

// kCGEventSourceStateHIDSystemState: events look like the keyboard and mouse.
const SOURCE_HID_SYSTEM_STATE: i32 = 1;
// kCGHIDEventTap.
const HID_EVENT_TAP: u32 = 0;
// kCGEventSourceStateCombinedSessionState and kCGAnyInputEventType.
const SESSION_STATE: i32 = 0;
const ANY_INPUT_EVENT: u32 = u32::MAX;
// kCGEventLeftMouseDown, ...Up, RightMouseDown, ...Up, MouseMoved, the dragged
// types, and the "other" button (middle) types.
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
// kCGMouseEventClickState, kCGMouseEventDeltaX and kCGMouseEventDeltaY.
const FIELD_CLICK_STATE: u32 = 1;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
// kCGScrollEventUnitPixel.
const SCROLL_PIXELS: u32 = 0;

/// An event that is released when it goes out of scope.
struct Event(EventRef);

impl Event {
    fn new(raw: EventRef) -> Option<Event> {
        (!raw.is_null()).then_some(Event(raw))
    }

    fn post(self) {
        // SAFETY: `self.0` is a live event that this value owns.
        unsafe { CGEventPost(HID_EVENT_TAP, self.0) }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a `CGEventCreate*` call (a "Create" function
        // returns an owned reference), and nothing uses it after this.
        unsafe { CFRelease(self.0.cast_const()) }
    }
}

/// The event source for posted events, made once. A null pointer (the source
/// could not be made) is allowed everywhere it is used.
fn source() -> SourceRef {
    static SOURCE: OnceLock<usize> = OnceLock::new();
    // SAFETY: the source is kept for the life of the process, and CoreGraphics
    // may use it from any thread.
    *SOURCE.get_or_init(|| unsafe { CGEventSourceCreate(SOURCE_HID_SYSTEM_STATE) } as usize) as SourceRef
}

pub fn post_access() -> bool {
    CGPreflightPostEventAccess()
}

pub fn request_post_access() -> bool {
    CGRequestPostEventAccess()
}

pub fn seconds_since_last_event() -> f64 {
    CGEventSourceSecondsSinceLastEventType(SESSION_STATE, ANY_INPUT_EVENT)
}

/// Where the cursor is now.
pub fn cursor() -> Option<Point> {
    // SAFETY: a null source is allowed, and the event is released by `Event`.
    let event = Event::new(unsafe { CGEventCreate(std::ptr::null_mut()) })?;
    // SAFETY: `event` is live.
    Some(unsafe { CGEventGetLocation(event.0) })
}

pub fn main_display_bounds() -> Rect {
    CGDisplayBounds(CGMainDisplayID()).into()
}

/// `point`, moved to the nearest place inside the box around all displays.
pub fn clamp_to_displays(point: Point) -> Point {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: `ids` has room for the 16 that we ask for, and `count` is a valid out pointer.
    let status = unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) };
    if status != 0 || count == 0 {
        return point;
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &id in &ids[..(count as usize).min(ids.len())] {
        let r: Rect = CGDisplayBounds(id).into();
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_x = max_x.max(r.x + r.width);
        max_y = max_y.max(r.y + r.height);
    }
    // The far edge belongs to no display, so the last point is one before it.
    Point {
        x: point.x.clamp(min_x, (max_x - 1.0).max(min_x)),
        y: point.y.clamp(min_y, (max_y - 1.0).max(min_y)),
    }
}

/// Moves the cursor to `to`. While a button is held (`dragging` is its index)
/// the event is a drag, so apps see the button down.
pub fn post_mouse_move(to: Point, dragging: Option<usize>, delta: (i64, i64)) {
    let (kind, button) = match dragging {
        None => (MOUSE_MOVED, 0),
        Some(0) => (LEFT_DRAGGED, 0),
        Some(1) => (RIGHT_DRAGGED, 1),
        Some(_) => (OTHER_DRAGGED, 2),
    };
    // SAFETY: the source may be null, and the event is released by `Event`.
    let Some(event) = Event::new(unsafe { CGEventCreateMouseEvent(source(), kind, to, button) }) else {
        return;
    };
    // SAFETY: `event` is live.
    unsafe {
        CGEventSetIntegerValueField(event.0, FIELD_DELTA_X, delta.0);
        CGEventSetIntegerValueField(event.0, FIELD_DELTA_Y, delta.1);
    }
    event.post();
}

/// Presses or releases mouse button `index` (0 left, 1 right, 2 middle) at `at`.
pub fn post_mouse_button(index: usize, down: bool, at: Point, click_state: i64) {
    let (kind, button) = match (index, down) {
        (0, true) => (LEFT_DOWN, 0),
        (0, false) => (LEFT_UP, 0),
        (1, true) => (RIGHT_DOWN, 1),
        (1, false) => (RIGHT_UP, 1),
        (_, true) => (OTHER_DOWN, 2),
        (_, false) => (OTHER_UP, 2),
    };
    // SAFETY: the source may be null, and the event is released by `Event`.
    let Some(event) = Event::new(unsafe { CGEventCreateMouseEvent(source(), kind, at, button) }) else {
        return;
    };
    // SAFETY: `event` is live.
    unsafe { CGEventSetIntegerValueField(event.0, FIELD_CLICK_STATE, click_state) };
    event.post();
}

/// Scrolls by `vertical` and `horizontal` pixels (positive is up and left).
pub fn post_scroll(vertical: i32, horizontal: i32) {
    // SAFETY: the source may be null, and the event is released by `Event`.
    let Some(event) = Event::new(unsafe {
        CGEventCreateScrollWheelEvent2(source(), SCROLL_PIXELS, 2, vertical, horizontal, 0)
    }) else {
        return;
    };
    event.post();
}

/// Presses or releases the key `code`, with the modifier `flags` set. With
/// `units` the event types those UTF-16 units, whatever the key code.
pub fn post_key(code: u16, down: bool, flags: u64, units: Option<&[u16]>) {
    // SAFETY: the source may be null, and the event is released by `Event`.
    let Some(event) = Event::new(unsafe { CGEventCreateKeyboardEvent(source(), code, down) }) else {
        return;
    };
    // SAFETY: `event` is live, and `units` stays valid for the call (the event copies it).
    unsafe {
        CGEventSetFlags(event.0, flags);
        if let Some(units) = units {
            CGEventKeyboardSetUnicodeString(event.0, units.len(), units.as_ptr());
        }
    }
    event.post();
}
