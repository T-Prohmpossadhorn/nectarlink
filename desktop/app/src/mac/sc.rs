// SPDX-License-Identifier: GPL-3.0-or-later
//! SystemConfiguration dynamic store notifications. The declarations follow
//! the macOS SDK headers (SCDynamicStore.h, CFBase.h, CFArray.h, CFString.h,
//! dispatch/queue.h). The store is delivered on a private serial dispatch
//! queue, so no run loop and no main thread is needed.

#![allow(unsafe_code)]

use std::{
    ffi::{CString, c_char, c_void},
    ptr,
};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type Boolean = u8;
type StoreRef = *const c_void;
type QueueRef = *mut c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Exact dynamic store keys that change when the default route changes.
pub const KEYS: [&str; 2] = ["State:/Network/Global/IPv4", "State:/Network/Global/IPv6"];

/// Regular expressions for per-interface address and link changes. The
/// header only says "regex(3)", so the form is a hardware check.
pub const PATTERNS: [&str; 1] = ["State:/Network/Interface/.*/(IPv4|IPv6|Link)"];

type Callout = unsafe extern "C" fn(store: StoreRef, changed_keys: CFTypeRef, info: *mut c_void);

#[repr(C)]
struct StoreContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: u8;

    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, c_str: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        call_backs: *const c_void,
    ) -> CFTypeRef;
}

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCDynamicStoreCreate(
        allocator: CFTypeRef,
        name: CFTypeRef,
        callout: Callout,
        context: *mut StoreContext,
    ) -> StoreRef;
    fn SCDynamicStoreSetNotificationKeys(store: StoreRef, keys: CFTypeRef, patterns: CFTypeRef) -> Boolean;
    fn SCDynamicStoreSetDispatchQueue(store: StoreRef, queue: QueueRef) -> Boolean;
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> QueueRef;
    fn dispatch_release(object: *mut c_void);
}

/// A CF string made from `text`, owned by the caller. `None` for a NUL byte
/// or a NULL return.
fn cf_string(text: &str) -> Option<CFTypeRef> {
    let c = CString::new(text).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string for the call.
    let s = unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8) };
    (!s.is_null()).then_some(s)
}

/// An owned CF array of CF strings (the strings are released here).
fn cf_string_array(items: &[&str]) -> Option<CFTypeRef> {
    let mut strings: Vec<CFTypeRef> = Vec::with_capacity(items.len());
    for item in items {
        match cf_string(item) {
            Some(s) => strings.push(s),
            None => {
                release_all(&strings);
                return None;
            }
        }
    }
    // SAFETY: `strings` holds valid CF strings; the array retains them.
    let array = unsafe {
        CFArrayCreate(
            ptr::null(),
            strings.as_ptr(),
            strings.len() as CFIndex,
            ptr::from_ref(&kCFTypeArrayCallBacks).cast(),
        )
    };
    release_all(&strings);
    (!array.is_null()).then_some(array)
}

fn release_all(items: &[CFTypeRef]) {
    for item in items {
        // SAFETY: each item is an owned CF object that is released once.
        unsafe { CFRelease(*item) };
    }
}

/// A running dynamic store subscription. Stop it with [`Subscription::stop`].
pub struct Subscription {
    store: StoreRef,
    queue: QueueRef,
}

// SAFETY: the raw references are only used to stop the subscription, once,
// and the store and queue are thread-safe CF and dispatch objects.
unsafe impl Send for Subscription {}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Subscription {
    /// Subscribes to [`KEYS`] and [`PATTERNS`]. `callback` runs on a private
    /// dispatch queue for every notification, including one with an empty
    /// changed-keys list (configd restarted). The callback is leaked on
    /// purpose: a notification can still be running after `stop`.
    pub fn start(callback: Box<dyn Fn() + Send + Sync>) -> Option<Self> {
        let boxed: *mut Callback = Box::into_raw(Box::new(callback));
        let name = cf_string("nectarlink")?;
        let mut context = StoreContext {
            version: 0,
            info: boxed.cast(),
            retain: ptr::null(),
            release: ptr::null(),
            copy_description: ptr::null(),
        };
        // SAFETY: `context` is copied by Create; `name` is valid for the call.
        let store = unsafe { SCDynamicStoreCreate(ptr::null(), name, on_change, &raw mut context) };
        release_all(&[name]);
        if store.is_null() {
            return None;
        }
        let subscription = Self::attach(store);
        if subscription.is_none() {
            // SAFETY: `store` is an owned reference.
            unsafe { CFRelease(store) };
        }
        subscription
    }

    fn attach(store: StoreRef) -> Option<Self> {
        let keys = cf_string_array(&KEYS)?;
        let Some(patterns) = cf_string_array(&PATTERNS) else {
            release_all(&[keys]);
            return None;
        };
        // SAFETY: store, keys and patterns are valid; the store copies the arrays.
        let set = unsafe { SCDynamicStoreSetNotificationKeys(store, keys, patterns) };
        release_all(&[keys, patterns]);
        if set == 0 {
            return None;
        }
        // SAFETY: the label is a NUL-terminated literal; NULL attr means serial.
        let queue = unsafe { dispatch_queue_create(c"app.nectarlink.net-watch".as_ptr(), ptr::null()) };
        if queue.is_null() {
            return None;
        }
        // SAFETY: store and queue are valid; the store retains the queue.
        if unsafe { SCDynamicStoreSetDispatchQueue(store, queue) } == 0 {
            // SAFETY: the queue was created here and is owned.
            unsafe { dispatch_release(queue) };
            return None;
        }
        Some(Self { store, queue })
    }

    /// Stops the notifications and releases the store and queue.
    pub fn stop(self) {
        // SAFETY: NULL detaches the queue; then the owned store and queue
        // references are released once.
        unsafe {
            SCDynamicStoreSetDispatchQueue(self.store, ptr::null_mut());
            CFRelease(self.store);
            dispatch_release(self.queue);
        }
    }
}

type Callback = Box<dyn Fn() + Send + Sync>;

unsafe extern "C" fn on_change(_store: StoreRef, _changed: CFTypeRef, info: *mut c_void) {
    // The changed-keys list is ignored on purpose: an empty list (configd
    // restart) counts as a change too. A panic must not cross the C frame.
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: `info` is the leaked Callback from `start`, never freed.
        let callback = unsafe { &*info.cast::<Callback>() };
        callback();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_patterns_are_exact() {
        assert_eq!(KEYS, ["State:/Network/Global/IPv4", "State:/Network/Global/IPv6"]);
        assert_eq!(PATTERNS, ["State:/Network/Interface/.*/(IPv4|IPv6|Link)"]);
    }

    #[test]
    fn cf_arrays_build_and_release() {
        // Pure CoreFoundation string work: no store, no prompt.
        let array = cf_string_array(&KEYS).expect("array");
        release_all(&[array]);
        assert!(cf_string("a\0b").is_none());
    }
}
