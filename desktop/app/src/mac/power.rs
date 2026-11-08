// SPDX-License-Identifier: GPL-3.0-or-later
//! Tells the app when the Mac wakes from sleep, so it can look for its phones
//! again. The declarations follow the macOS SDK headers `IOKit/pwr_mgt/IOPMLib.h`,
//! `IOKit/IOMessage.h`, `IOKit/IOReturn.h` and `CoreFoundation/CFRunLoop.h`.
//! Watching sleep and wake needs no permission.
//!
//! One thread owns a run loop with the power-notification source on it. The
//! system asks the app about a coming sleep and waits for an answer, so the
//! callback must always answer those two messages (`IOAllowPowerChange`),
//! or the Mac waits 30 seconds before it sleeps. It must not answer any
//! other message.

#![allow(unsafe_code)]

use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
};

/// `io_object_t`, `io_connect_t` and `io_service_t` are all a `mach_port_t`.
type IoObject = u32;
type NotifyPort = *mut c_void;
type RunLoop = *mut c_void;
type RunLoopSource = *mut c_void;
type RunLoopMode = *const c_void;
type ServiceCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    service: IoObject,
    message_type: u32,
    message_argument: *mut c_void,
);

/// `iokit_common_msg(m)` of `IOKit/IOMessage.h`: `sys_iokit | sub_iokit_common | m`,
/// where `sys_iokit` is `err_system(0x38)` and `sub_iokit_common` is `err_sub(0)`
/// (`mach/error.h`).
const fn iokit_common_msg(message: u32) -> u32 {
    const SYS_IOKIT: u32 = (0x38 & 0x3f) << 26;
    const SUB_IOKIT_COMMON: u32 = 0;
    SYS_IOKIT | SUB_IOKIT_COMMON | message
}

/// The system asks whether it may sleep (idle sleep). Answer it.
const CAN_SYSTEM_SLEEP: u32 = iokit_common_msg(0x270);
/// Another program said no to the idle sleep. No answer.
const SYSTEM_WILL_NOT_SLEEP: u32 = iokit_common_msg(0x290);
/// The system goes to sleep now. Answer it.
const SYSTEM_WILL_SLEEP: u32 = iokit_common_msg(0x280);
/// Early in the wake. Hardware is not ready. No answer.
const SYSTEM_WILL_POWER_ON: u32 = iokit_common_msg(0x320);
/// The wake is done. No answer.
const SYSTEM_HAS_POWERED_ON: u32 = iokit_common_msg(0x300);

/// How long one turn of the run loop waits, in seconds. After it the thread
/// looks at its stop flag, so a missed wake-up call cannot hold up the join.
const RUN_SLICE_SECS: f64 = 0.5;

/// What the callback does with one power message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowerAction {
    /// Call `IOAllowPowerChange`.
    Acknowledge,
    /// The Mac woke up. Do not answer.
    Resumed,
    /// Nothing. Do not answer.
    Ignore,
}

/// The action for a message type.
pub(crate) fn action_for_message(message_type: u32) -> PowerAction {
    match message_type {
        CAN_SYSTEM_SLEEP | SYSTEM_WILL_SLEEP => PowerAction::Acknowledge,
        SYSTEM_HAS_POWERED_ON => PowerAction::Resumed,
        // The header says: do not answer these.
        SYSTEM_WILL_NOT_SLEEP | SYSTEM_WILL_POWER_ON => PowerAction::Ignore,
        _ => PowerAction::Ignore,
    }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut NotifyPort,
        callback: ServiceCallback,
        notifier: *mut IoObject,
    ) -> IoObject;
    fn IODeregisterForSystemPower(notifier: *mut IoObject) -> i32;
    fn IOAllowPowerChange(kernel_port: IoObject, notification_id: isize) -> i32;
    fn IOServiceClose(connect: IoObject) -> i32;
    fn IONotificationPortGetRunLoopSource(port: NotifyPort) -> RunLoopSource;
    fn IONotificationPortDestroy(port: NotifyPort);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: RunLoopMode;
    fn CFRunLoopGetCurrent() -> RunLoop;
    fn CFRunLoopAddSource(run_loop: RunLoop, source: RunLoopSource, mode: RunLoopMode);
    fn CFRunLoopRemoveSource(run_loop: RunLoop, source: RunLoopSource, mode: RunLoopMode);
    fn CFRunLoopRunInMode(mode: RunLoopMode, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRunLoopStop(run_loop: RunLoop);
}

/// What the callback needs. It lives from the registration to the cleanup.
struct Context {
    /// The connection `IORegisterForSystemPower` returns. It is only known
    /// after the registration, and the callback needs it to answer.
    root_port: AtomicU32,
    on_resume: Box<dyn Fn() + Send + Sync>,
}

unsafe extern "C" fn on_power_message(
    refcon: *mut c_void,
    _service: IoObject,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // SAFETY: `refcon` is the pointer to the `Context` that `run` made. `run`
    // frees it only after the registration is gone, so it is alive here.
    let context = unsafe { &*refcon.cast::<Context>() };
    match action_for_message(message_type) {
        PowerAction::Acknowledge => {
            let port = context.root_port.load(Ordering::Acquire);
            if port != 0 {
                // SAFETY: `port` is the live connection of this registration.
                // The argument is the notification id the system gave us.
                unsafe { IOAllowPowerChange(port, message_argument as isize) };
            }
        }
        PowerAction::Resumed => {
            // A panic must not cross into the system's code.
            let _ = catch_unwind(AssertUnwindSafe(|| (context.on_resume)()));
        }
        PowerAction::Ignore => {}
    }
}

/// Registers, runs the loop until `stop` is set, and cleans up. `run_loop`
/// holds the thread's run loop while it runs, so the owner can wake it.
fn run(stop: &AtomicBool, run_loop: &Mutex<usize>, on_resume: Box<dyn Fn() + Send + Sync>) {
    let context = Box::into_raw(Box::new(Context { root_port: AtomicU32::new(0), on_resume }));
    let mut port: NotifyPort = ptr::null_mut();
    let mut notifier: IoObject = 0;
    // SAFETY: the pointers are to live locals, and `context` stays alive
    // until the end of this function.
    let root =
        unsafe { IORegisterForSystemPower(context.cast(), &mut port, on_power_message, &mut notifier) };
    if root == 0 || port.is_null() {
        tracing::warn!("can't watch for sleep and wake");
        // SAFETY: nothing was registered, so nothing else holds `context`.
        drop(unsafe { Box::from_raw(context) });
        return;
    }
    // SAFETY: `context` is live. It is stored before the loop can deliver a message.
    unsafe { &*context }.root_port.store(root, Ordering::Release);

    // SAFETY: `port` is live. The source belongs to the port: it is not released here.
    let source = unsafe { IONotificationPortGetRunLoopSource(port) };
    // SAFETY: this thread's own run loop, and the default mode is a constant.
    let (current, mode) = unsafe { (CFRunLoopGetCurrent(), kCFRunLoopDefaultMode) };
    if !source.is_null() {
        // SAFETY: all three are live.
        unsafe { CFRunLoopAddSource(current, source, mode) };
        *run_loop.lock().unwrap_or_else(|e| e.into_inner()) = current as usize;
        while !stop.load(Ordering::Acquire) {
            // SAFETY: runs this thread's run loop for a short time.
            unsafe { CFRunLoopRunInMode(mode, RUN_SLICE_SECS, 0) };
        }
        // After this the owner no longer calls `CFRunLoopStop` on the loop.
        *run_loop.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        // SAFETY: the source was added above.
        unsafe { CFRunLoopRemoveSource(current, source, mode) };
    } else {
        tracing::warn!("can't watch for sleep and wake");
    }
    // The order the header asks for: deregister, close the connection, then
    // destroy the port (that also frees the source).
    // SAFETY: each handle is live and used once. After `IODeregisterForSystemPower`
    // no callback runs, so `context` can go.
    unsafe {
        IODeregisterForSystemPower(&mut notifier);
        IOServiceClose(root);
        IONotificationPortDestroy(port);
        drop(Box::from_raw(context));
    }
}

/// The thread that watches for wake. Dropping it stops the thread and waits
/// for it.
pub struct PowerWatcher {
    stop: Arc<AtomicBool>,
    run_loop: Arc<Mutex<usize>>,
    thread: Option<JoinHandle<()>>,
}

impl PowerWatcher {
    /// Starts the thread. `on_resume` runs on that thread after each wake, so
    /// it must be quick. `None` when the thread cannot start. A failed
    /// registration is only logged: the app works without wake events.
    pub fn start(on_resume: impl Fn() + Send + Sync + 'static) -> Option<PowerWatcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let run_loop = Arc::new(Mutex::new(0usize));
        let thread = {
            let stop = Arc::clone(&stop);
            let run_loop = Arc::clone(&run_loop);
            std::thread::Builder::new()
                .name("mac-power".into())
                .spawn(move || run(&stop, &run_loop, Box::new(on_resume)))
                .ok()?
        };
        Some(PowerWatcher { stop, run_loop, thread: Some(thread) })
    }
}

impl Drop for PowerWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            // The thread clears this under the same lock before it leaves the
            // loop, so a non-zero value is a run loop that is still alive.
            let run_loop = self.run_loop.lock().unwrap_or_else(|e| e.into_inner());
            if *run_loop != 0 {
                // SAFETY: the run loop is alive (see above), and
                // `CFRunLoopStop` may be called from any thread.
                unsafe { CFRunLoopStop(*run_loop as RunLoop) };
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_numbers_follow_the_header_formula() {
        // `sys_iokit` is 0x38 << 26 = 0xE0000000. The values are those in IOMessage.h.
        assert_eq!(CAN_SYSTEM_SLEEP, 0xE000_0270);
        assert_eq!(SYSTEM_WILL_SLEEP, 0xE000_0280);
        assert_eq!(SYSTEM_WILL_NOT_SLEEP, 0xE000_0290);
        assert_eq!(SYSTEM_HAS_POWERED_ON, 0xE000_0300);
        assert_eq!(SYSTEM_WILL_POWER_ON, 0xE000_0320);
    }

    #[test]
    fn a_coming_sleep_is_answered_and_a_wake_is_not() {
        assert_eq!(action_for_message(CAN_SYSTEM_SLEEP), PowerAction::Acknowledge);
        assert_eq!(action_for_message(SYSTEM_WILL_SLEEP), PowerAction::Acknowledge);
        assert_eq!(action_for_message(SYSTEM_HAS_POWERED_ON), PowerAction::Resumed);
    }

    #[test]
    fn every_other_message_is_left_alone() {
        for message in
            [SYSTEM_WILL_NOT_SLEEP, SYSTEM_WILL_POWER_ON, iokit_common_msg(0x255), 0, 0x270, u32::MAX]
        {
            assert_eq!(action_for_message(message), PowerAction::Ignore, "{message:#x}");
        }
    }
}
