// SPDX-License-Identifier: GPL-3.0-or-later
//! The CoreGraphics and CoreFoundation calls behind `mac/browser.rs`, with
//! the `unsafe` kept here. The window list is copied, read into plain Rust
//! values and released inside one function. No function keeps a pointer.
//!
//! No call here asks for a permission on purpose. We have not proven that
//! none of them raises a prompt. For example, `CFBundleCreate` reads
//! `Contents/Info.plist`, and it is a hardware check not yet done whether
//! that raises a Files and Folders prompt for an app in a protected folder.
//! A window's title may be left out by the system when Screen Recording is
//! not granted. That is not proven either, so a missing title is read as an
//! empty one and nothing here checks the permission.

#![allow(unsafe_code)]

use std::{
    ffi::{CStr, c_char, c_void},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use super::Record;

type CFTypeRef = *const c_void;

#[derive(Clone, Copy)]
#[repr(C)]
struct RawRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

// kCGWindowListOptionOnScreenOnly, ...IncludingWindow and
// ...ExcludeDesktopElements. The first lists the windows on screen from front
// to back. `kCGWindowListOptionAll` (zero) is not used: its order is not
// documented.
pub const ON_SCREEN_ONLY: u32 = 1 << 0;
pub const INCLUDING_WINDOW: u32 = 1 << 3;
pub const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

// kCFNumberSInt32Type, kCFStringEncodingUTF8.
const NUMBER_SINT32: isize = 3;
const UTF8: u32 = 0x0800_0100;
/// `PROC_PIDPATHINFO_MAXSIZE`: four times `MAXPATHLEN`.
const PATH_MAX: usize = 4096;
/// A window title longer than this (in UTF-8 bytes) is read as empty. No real
/// title is near it, and it keeps one odd window from costing a big buffer.
const TITLE_MAX: isize = 64 * 1024;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGWindowNumber: CFTypeRef;
    static kCGWindowLayer: CFTypeRef;
    static kCGWindowOwnerPID: CFTypeRef;
    static kCGWindowBounds: CFTypeRef;
    static kCGWindowName: CFTypeRef;
    /// A +1 array of window dictionaries (the "Copy" rule), or null when there
    /// is no GUI session.
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
    fn CGRectMakeWithDictionaryRepresentation(dictionary: CFTypeRef, rect: *mut RawRect) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CFTypeRef);
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(string: CFTypeRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(string: CFTypeRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFTypeRef,
        buffer: *const u8,
        length: isize,
        is_directory: u8,
    ) -> CFTypeRef;
    fn CFBundleCreate(allocator: CFTypeRef, url: CFTypeRef) -> CFTypeRef;
    fn CFBundleGetIdentifier(bundle: CFTypeRef) -> CFTypeRef;
}

unsafe extern "C" {
    /// From libproc (part of libSystem). Returns the length written, or 0.
    fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
}

/// A CoreFoundation object this value owns, released when it goes out of scope.
struct Owned(CFTypeRef);

impl Owned {
    /// Takes the +1 reference of a "Copy" or "Create" call. Null gives `None`.
    fn new(raw: CFTypeRef) -> Option<Owned> {
        (!raw.is_null()).then_some(Owned(raw))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null reference from a Copy or Create call,
        // which this value owns, and nothing uses it after this.
        unsafe { CFRelease(self.0) }
    }
}

/// An entry of `dictionary` when it exists and has the type `type_id` gives.
/// The value stays borrowed from the dictionary.
fn entry(dictionary: CFTypeRef, key: CFTypeRef, type_id: usize) -> Option<CFTypeRef> {
    // SAFETY: both arguments are live CoreFoundation objects.
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    // SAFETY: a non-null value in a dictionary is a live CoreFoundation object.
    (!value.is_null() && unsafe { CFGetTypeID(value) } == type_id).then_some(value)
}

fn int_entry(dictionary: CFTypeRef, key: CFTypeRef) -> Option<i32> {
    // SAFETY: the type id call has no arguments.
    let number = entry(dictionary, key, unsafe { CFNumberGetTypeID() })?;
    let mut out = 0i32;
    // SAFETY: `number` is a CFNumber (checked), and `out` holds an SInt32.
    let exact = unsafe { CFNumberGetValue(number, NUMBER_SINT32, (&raw mut out).cast()) };
    (exact != 0).then_some(out)
}

fn bounds_entry(dictionary: CFTypeRef) -> Option<[f64; 4]> {
    // SAFETY: the key is an exported constant; the type id call has no arguments.
    let bounds = entry(dictionary, unsafe { kCGWindowBounds }, unsafe { CFDictionaryGetTypeID() })?;
    let mut rect = RawRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
    // SAFETY: `bounds` is a CFDictionary (checked) and `rect` is a CGRect.
    let made = unsafe { CGRectMakeWithDictionaryRepresentation(bounds, &raw mut rect) };
    made.then_some([rect.x, rect.y, rect.width, rect.height])
}

/// The text of a CFString as UTF-8, or `None` when it is too long or cannot be
/// converted.
fn string_value(string: CFTypeRef) -> Option<String> {
    // SAFETY: `string` is a live CFString (the caller checked its type).
    let size = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(string), UTF8) };
    if !(0..=TITLE_MAX).contains(&size) {
        return None;
    }
    let mut buffer = vec![0u8; usize::try_from(size).ok()? + 1];
    // SAFETY: the buffer holds `size + 1` bytes, which fits the longest result
    // and its NUL; `string` is a live CFString.
    let written = unsafe { CFStringGetCString(string, buffer.as_mut_ptr().cast(), size + 1, UTF8) };
    if written == 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buffer).ok()?;
    Some(text.to_string_lossy().into_owned())
}

fn title_entry(dictionary: CFTypeRef) -> Option<String> {
    // SAFETY: the key is an exported constant; the type id call has no arguments.
    let title = entry(dictionary, unsafe { kCGWindowName }, unsafe { CFStringGetTypeID() })?;
    string_value(title)
}

/// The windows `option` lists, relative to `relative_to`, as plain records.
/// No GUI session (the copy is null) gives an empty list.
pub fn window_records(option: u32, relative_to: u32) -> Vec<Record> {
    // SAFETY: CGWindowListCopyWindowInfo follows the Copy rule: one reference
    // that we own, released by the guard. It takes no pointers.
    let Some(list) = Owned::new(unsafe { CGWindowListCopyWindowInfo(option, relative_to) }) else {
        return Vec::new();
    };
    // SAFETY: `list.0` is a live CFArray.
    let count = unsafe { CFArrayGetCount(list.0) };
    let mut records = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is below the count; the item stays borrowed from the array.
        let item = unsafe { CFArrayGetValueAtIndex(list.0, index) };
        // SAFETY: the type id call has no arguments; `item` is checked for null first.
        if item.is_null() || unsafe { CFGetTypeID(item) != CFDictionaryGetTypeID() } {
            continue;
        }
        // SAFETY: the keys are exported constants.
        let (number, layer, pid) = unsafe { (kCGWindowNumber, kCGWindowLayer, kCGWindowOwnerPID) };
        records.push(Record {
            number: int_entry(item, number),
            layer: int_entry(item, layer),
            pid: int_entry(item, pid),
            bounds: bounds_entry(item),
            title: title_entry(item),
        });
    }
    records
}

/// The path of the executable that process `pid` runs, or `None` when the
/// system will not say (the process ended, or it is not ours to ask about).
pub fn exe_path(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; PATH_MAX];
    // SAFETY: the buffer holds `PATH_MAX` bytes and the call writes at most that.
    let written = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), PATH_MAX as u32) };
    let written = usize::try_from(written).ok().filter(|n| *n > 0)?;
    buffer.truncate(written);
    // A NUL ends the path if the system wrote one inside the count.
    if let Some(nul) = buffer.iter().position(|b| *b == 0) {
        buffer.truncate(nul);
    }
    (!buffer.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

/// The bundle id of the app folder `app`, as the app's Info.plist spells it.
/// `None` when the folder is not a bundle or has no id.
pub fn bundle_id(app: &Path) -> Option<String> {
    let bytes = app.as_os_str().as_bytes();
    let length = isize::try_from(bytes.len()).ok()?;
    // SAFETY: the buffer is live for the call, which copies it; null picks the
    // default allocator. A "Create" call: the guard releases the URL.
    let url = Owned::new(unsafe {
        CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), length, 1)
    })?;
    // SAFETY: `url.0` is a live CFURL. A "Create" call: the guard releases the
    // bundle (null when the folder is not a bundle).
    let bundle = Owned::new(unsafe { CFBundleCreate(std::ptr::null(), url.0) })?;
    // SAFETY: `bundle.0` is a live CFBundle. A "Get" call: the id stays
    // borrowed from the bundle, so it is copied below before the bundle goes.
    let id = unsafe { CFBundleGetIdentifier(bundle.0) };
    // SAFETY: the type id call has no arguments; null is checked first.
    if id.is_null() || unsafe { CFGetTypeID(id) != CFStringGetTypeID() } {
        return None;
    }
    string_value(id)
}
