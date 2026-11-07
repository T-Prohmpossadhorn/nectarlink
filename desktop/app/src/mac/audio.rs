// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS speaker volume, mute, and state, and the output devices. The device
//! calls follow the SDK headers `CoreAudio/AudioHardware.h` and
//! `CoreAudio/AudioHardwareBase.h` and need no permission.

#![allow(unsafe_code)]

use std::{
    ffi::{CStr, c_char, c_void},
    mem::size_of,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::Duration,
};

fn parse_speaker_state(text: &str) -> Option<(u8, bool)> {
    let mut parts = text.split_whitespace();
    let vol: u8 = parts.next()?.parse().ok()?;
    let muted: bool = parts.next()?.parse().unwrap_or(false);
    Some((vol, muted))
}

pub fn speaker_state() -> Option<(u8, bool)> {
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            "set v to get volume settings",
            "-e",
            "(output volume of v as string) & \" \" & (output muted of v as string)",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_speaker_state(&String::from_utf8_lossy(&output.stdout))
}

fn run_osascript(script: &str) -> Result<(), String> {
    let output =
        std::process::Command::new("osascript").args(["-e", script]).output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("osascript failed with exit code {:?}", output.status.code())
        } else {
            msg.to_string()
        });
    }
    Ok(())
}

pub fn set_speaker_volume(level: u8) -> Result<(), String> {
    run_osascript(&format!("set volume output volume {level}"))
}

pub fn set_speaker_mute(muted: bool) -> Result<(), String> {
    run_osascript(&format!("set volume output muted {muted}"))
}

pub fn toggle_speaker_mute() -> Result<bool, String> {
    let (_, muted) = speaker_state().unwrap_or((50, false));
    let next = !muted;
    set_speaker_mute(next)?;
    Ok(next)
}

pub fn speaker_volume_up() -> Result<(u8, bool), String> {
    let (vol, muted) = speaker_state().unwrap_or((50, false));
    let next = vol.saturating_add(5).min(100);
    set_speaker_volume(next)?;
    Ok((next, muted))
}

pub fn speaker_volume_down() -> Result<(u8, bool), String> {
    let (vol, muted) = speaker_state().unwrap_or((50, false));
    let next = vol.saturating_sub(5);
    set_speaker_volume(next)?;
    Ok((next, muted))
}

static LAST_MIC_VOL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(75);

pub fn mic_volume() -> Option<u8> {
    let output = std::process::Command::new("osascript")
        .args(["-e", "input volume of (get volume settings)"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse().ok()
}

pub fn mic_muted() -> Option<bool> {
    mic_volume().map(|vol| vol == 0)
}

pub fn toggle_mic_mute() -> Result<bool, String> {
    let current_vol = mic_volume().ok_or_else(|| "Couldn't read microphone volume".to_string())?;
    let muted = current_vol == 0;
    let next = !muted;
    let vol = if next {
        if current_vol > 0 {
            LAST_MIC_VOL.store(current_vol, std::sync::atomic::Ordering::Relaxed);
        }
        0
    } else {
        let saved = LAST_MIC_VOL.load(std::sync::atomic::Ordering::Relaxed);
        if saved > 0 { saved } else { 75 }
    };
    let script = format!("set volume input volume {vol}");
    run_osascript(&script)?;
    Ok(next)
}

/// A four-character code as the SDK headers write it (`'dev#'`), most
/// significant byte first.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

type OSStatus = i32;
type AudioObjectID = u32;
type CFTypeRef = *const c_void;
type CFIndex = isize;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kAudioObjectSystemObject`.
const SYSTEM_OBJECT: AudioObjectID = 1;
/// `kAudioHardwarePropertyDevices`: the `AudioObjectID`s of all devices.
const DEVICES: u32 = fourcc(b"dev#");
/// `kAudioHardwarePropertyDefaultOutputDevice`.
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
/// `kAudioDevicePropertyStreams`: one entry per stream in the scope.
const STREAMS: u32 = fourcc(b"stm#");
/// `kAudioDevicePropertyDeviceUID` and `kAudioObjectPropertyName`: `CFStringRef`s
/// the caller releases.
const DEVICE_UID: u32 = fourcc(b"uid ");
const NAME: u32 = fourcc(b"lnam");
/// `kAudioDevicePropertyTransportType`, and `kAudioDeviceTransportTypeVirtual`.
const TRANSPORT_TYPE: u32 = fourcc(b"tran");
const TRANSPORT_VIRTUAL: u32 = fourcc(b"virt");
/// `kAudioObjectPropertyScopeGlobal` and `kAudioObjectPropertyScopeOutput`.
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
/// `kAudioObjectPropertyElementMain`.
const ELEMENT_MAIN: u32 = 0;

/// `AudioObjectPropertyAddress`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

impl Address {
    const fn new(selector: u32, scope: u32) -> Address {
        Address { selector, scope, element: ELEMENT_MAIN }
    }
}

type Listener = unsafe extern "C" fn(
    object: AudioObjectID,
    count: u32,
    addresses: *const Address,
    client: *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectAddPropertyListener(
        object: AudioObjectID,
        address: *const Address,
        listener: Listener,
        client: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectRemovePropertyListener(
        object: AudioObjectID,
        address: *const Address,
        listener: Listener,
        client: *mut c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFStringGetCString(s: CFTypeRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> u8;
}

/// The size in bytes of a property, `None` if the object lacks it.
fn property_size(object: AudioObjectID, address: Address) -> Option<u32> {
    let mut size = 0u32;
    // SAFETY: `address` and `size` are valid for the call; no qualifier is used.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &raw const address, 0, ptr::null(), &raw mut size) };
    (status == 0).then_some(size)
}

/// A property that is one `u32`.
fn property_u32(object: AudioObjectID, address: Address) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` has room for `size` bytes, which the call may lower.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &raw const address,
            0,
            ptr::null(),
            &raw mut size,
            (&raw mut value).cast(),
        )
    };
    (status == 0 && size == size_of::<u32>() as u32).then_some(value)
}

/// A property that is a list of `u32`s (object IDs).
fn property_ids(object: AudioObjectID, address: Address) -> Vec<u32> {
    let Some(bytes) = property_size(object, address) else { return Vec::new() };
    let mut ids = vec![0u32; bytes as usize / size_of::<u32>()];
    let mut size = (ids.len() * size_of::<u32>()) as u32;
    // SAFETY: `ids` has room for `size` bytes, which the call may lower.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &raw const address,
            0,
            ptr::null(),
            &raw mut size,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / size_of::<u32>());
    ids
}

/// An owned (Create or Copy rule) CF object, released on drop.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the guard owns one non-null reference and releases it once.
        unsafe { CFRelease(self.0) };
    }
}

/// A property that is a `CFStringRef` (Copy rule: released here), as UTF-8.
/// `None` for no value, or one that does not fit, so a UID is never cut short.
fn property_string(object: AudioObjectID, address: Address) -> Option<String> {
    let mut string: CFTypeRef = ptr::null();
    let mut size = size_of::<CFTypeRef>() as u32;
    // SAFETY: `string` has room for one reference. The property is a string the
    // caller owns, released once by `Owned`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &raw const address,
            0,
            ptr::null(),
            &raw mut size,
            (&raw mut string).cast(),
        )
    };
    if status != 0 || string.is_null() {
        return None;
    }
    let string = Owned(string);
    let mut buf = [0 as c_char; 1024];
    // SAFETY: `string` is a valid CF string; `buf` is writable for its length.
    let ok = unsafe {
        CFStringGetCString(string.0, buf.as_mut_ptr(), buf.len() as CFIndex, CF_STRING_ENCODING_UTF8)
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: a successful call wrote a NUL-terminated string into `buf`.
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(text.to_string_lossy().into_owned())
}

/// What is known about one audio device before the list is made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    uid: String,
    name: String,
    /// It has at least one output stream (an input-only device does not).
    has_output: bool,
    /// Its transport type is virtual (BlackHole, Loopback, Soundflower).
    is_virtual: bool,
    is_default: bool,
}

/// The outputs among `candidates`: the default first, then real devices
/// before virtual ones, each group by name. Devices without a UID or a name
/// are left out.
fn assemble(candidates: Vec<Candidate>) -> Vec<nectarlink_core::AudioOutputDevice> {
    let mut outputs: Vec<Candidate> =
        candidates.into_iter().filter(|c| c.has_output && !c.uid.is_empty() && !c.name.is_empty()).collect();
    outputs.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.is_virtual.cmp(&b.is_virtual))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.uid.cmp(&b.uid))
    });
    outputs
        .into_iter()
        .map(|c| nectarlink_core::AudioOutputDevice { id: c.uid, name: c.name, is_default: c.is_default })
        .collect()
}

fn candidate(device: AudioObjectID, default: Option<AudioObjectID>) -> Candidate {
    // A device with no streams in the output scope cannot play: skip the
    // string reads for it.
    let has_output = property_size(device, Address::new(STREAMS, SCOPE_OUTPUT)).is_some_and(|size| size > 0);
    let (uid, name, is_virtual) = if has_output {
        (
            property_string(device, Address::new(DEVICE_UID, SCOPE_GLOBAL)).unwrap_or_default(),
            property_string(device, Address::new(NAME, SCOPE_GLOBAL)).unwrap_or_default(),
            property_u32(device, Address::new(TRANSPORT_TYPE, SCOPE_GLOBAL)) == Some(TRANSPORT_VIRTUAL),
        )
    } else {
        Default::default()
    };
    Candidate { uid, name, has_output, is_virtual, is_default: default == Some(device) }
}

/// Lists the output devices of this Mac (by UID), the default one first. Needs
/// no permission and no run loop.
pub fn output_devices() -> Vec<nectarlink_core::AudioOutputDevice> {
    let default = property_u32(SYSTEM_OBJECT, Address::new(DEFAULT_OUTPUT, SCOPE_GLOBAL));
    let candidates = property_ids(SYSTEM_OBJECT, Address::new(DEVICES, SCOPE_GLOBAL))
        .into_iter()
        .map(|device| candidate(device, default))
        .collect();
    assemble(candidates)
}

/// Whether [`watch`] has started.
static WATCHING: AtomicBool = AtomicBool::new(false);
/// How long the changes of a burst are gathered before `changed` is called.
const SETTLE: Duration = Duration::from_millis(150);

/// What the watch reports: a device added or removed, or a new default output.
const WATCHED: [Address; 2] =
    [Address::new(DEVICES, SCOPE_GLOBAL), Address::new(DEFAULT_OUTPUT, SCOPE_GLOBAL)];

/// Listeners on the system object. Dropping it takes them off again.
struct Registered {
    /// The leaked `Sender` the listeners were given.
    client: *mut c_void,
    added: Vec<Address>,
}

impl Registered {
    fn new(sender: Sender<()>) -> Option<Registered> {
        // Leaked on purpose, like the callback in `sc.rs`: a notification can
        // still be running after a listener is removed. It is one small value
        // per watch, and a watch that started is never started again. Only a
        // registration that fails before any listener was added frees it.
        let client: *mut c_void = Box::into_raw(Box::new(sender)).cast();
        let mut registered = Registered { client, added: Vec::new() };
        for address in WATCHED {
            // SAFETY: the address is valid for the call; `on_change` only sends
            // on the channel behind `client`, which is never freed.
            let status = unsafe {
                AudioObjectAddPropertyListener(SYSTEM_OBJECT, &raw const address, on_change, client)
            };
            if status != 0 {
                // Dropping `registered` removes the ones that were added.
                if registered.added.is_empty() {
                    drop(registered);
                    // SAFETY: no listener was ever added, so nothing else
                    // can reach `client`; it came from `Box::into_raw` above.
                    drop(unsafe { Box::from_raw(client.cast::<Sender<()>>()) });
                }
                return None;
            }
            registered.added.push(address);
        }
        Some(registered)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        for address in &self.added {
            // SAFETY: each listener was added with the same address and client.
            unsafe {
                AudioObjectRemovePropertyListener(SYSTEM_OBJECT, address, on_change, self.client);
            }
        }
    }
}

// SAFETY: `client` points to a leaked `Sender`, which is `Send` and `Sync`.
unsafe impl Send for Registered {}

/// Runs on the HAL's notification thread: only sends, so it is short.
unsafe extern "C" fn on_change(
    _object: AudioObjectID,
    _count: u32,
    _addresses: *const Address,
    client: *mut c_void,
) -> OSStatus {
    // A panic must not cross the C frame.
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: `client` is the leaked `Sender` from `Registered::new`.
        let sender = unsafe { &*client.cast::<Sender<()>>() };
        let _ = sender.send(());
    });
    0
}

/// Calls `changed` (on a worker thread, a moment after a burst of changes)
/// when an output device is plugged in or out or the default output changes.
/// Volume and mute changes are not watched on a Mac: the Deck re-reads them
/// only while its page is open.
/// Starts once; later calls do nothing.
pub fn watch(changed: fn()) {
    if WATCHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let Some(registered) = Registered::new(sender) else {
        tracing::warn!("can't watch the audio devices");
        WATCHING.store(false, Ordering::Release);
        return;
    };
    let worker = std::thread::Builder::new().name("audio-watch".into()).spawn(move || {
        let _registered = registered;
        while receiver.recv().is_ok() {
            // Settle: a device switch sends several at once.
            while receiver.recv_timeout(SETTLE).is_ok() {}
            changed();
        }
    });
    if worker.is_err() {
        // The closure and the listeners were dropped with it.
        WATCHING.store(false, Ordering::Release);
    }
}

#[allow(dead_code)]
pub fn start(_on_changed: impl Fn() + Send + Sync + 'static) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_state_parses_volume_and_mute() {
        assert_eq!(parse_speaker_state("25 false"), Some((25, false)));
        assert_eq!(parse_speaker_state("100 true\n"), Some((100, true)));
        assert_eq!(parse_speaker_state("0 false"), Some((0, false)));
        assert_eq!(parse_speaker_state(""), None);
        assert_eq!(parse_speaker_state("invalid"), None);
    }

    fn candidate(uid: &str, name: &str, has_output: bool, is_virtual: bool, is_default: bool) -> Candidate {
        Candidate { uid: uid.into(), name: name.into(), has_output, is_virtual, is_default }
    }

    fn ids(devices: &[nectarlink_core::AudioOutputDevice]) -> Vec<&str> {
        devices.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn four_character_codes_match_the_headers() {
        assert_eq!(DEVICES, 0x6465_7623);
        assert_eq!(DEFAULT_OUTPUT, 0x644F_7574);
        assert_eq!(STREAMS, 0x7374_6D23);
        assert_eq!(DEVICE_UID, 0x7569_6420, "the code has a trailing space");
        assert_eq!(NAME, 0x6C6E_616D);
        assert_eq!(TRANSPORT_TYPE, 0x7472_616E);
        assert_eq!(TRANSPORT_VIRTUAL, 0x7669_7274);
        assert_eq!(SCOPE_GLOBAL, 0x676C_6F62);
        assert_eq!(SCOPE_OUTPUT, 0x6F75_7470);
        assert_eq!(size_of::<Address>(), 12);
        assert_eq!(Address::new(DEVICES, SCOPE_GLOBAL).element, 0);
    }

    #[test]
    fn the_device_list_has_outputs_only_with_the_default_first() {
        let devices = assemble(vec![
            candidate("mic", "MacBook Pro Microphone", false, false, false),
            candidate("usb", "USB Headset", true, false, false),
            candidate("spk", "MacBook Pro Speakers", true, false, true),
            candidate("hdmi", "Display Audio", true, false, false),
        ]);
        assert_eq!(
            ids(&devices),
            ["spk", "hdmi", "usb"],
            "no input-only device, default first, then by name"
        );
        assert!(devices[0].is_default);
        assert!(devices[1..].iter().all(|d| !d.is_default));
        assert_eq!(devices[0].name, "MacBook Pro Speakers");
    }

    #[test]
    fn virtual_devices_come_after_real_ones_unless_they_are_the_default() {
        let list = || {
            vec![
                candidate("bh", "BlackHole 2ch", true, true, false),
                candidate("usb", "USB Headset", true, false, false),
                candidate("lb", "Loopback Audio", true, true, false),
                candidate("spk", "Speakers", true, false, false),
            ]
        };
        assert_eq!(ids(&assemble(list())), ["spk", "usb", "bh", "lb"]);
        let mut with_default = list();
        with_default[0].is_default = true;
        assert_eq!(ids(&assemble(with_default)), ["bh", "spk", "usb", "lb"]);
    }

    #[test]
    fn devices_without_a_uid_or_a_name_are_left_out() {
        let devices = assemble(vec![
            candidate("", "No UID", true, false, false),
            candidate("no-name", "", true, false, false),
            candidate("ok", "Fine", true, false, false),
        ]);
        assert_eq!(ids(&devices), ["ok"]);
        assert!(assemble(Vec::new()).is_empty());
    }

    #[test]
    fn the_sort_is_stable_for_equal_names() {
        let devices = assemble(vec![
            candidate("b", "Same", true, false, false),
            candidate("a", "Same", true, false, false),
        ]);
        assert_eq!(ids(&devices), ["a", "b"]);
    }

    #[test]
    fn query_live_speaker_state() {
        if let Some((vol, _muted)) = speaker_state() {
            assert!(vol <= 100);
        }
    }
}
