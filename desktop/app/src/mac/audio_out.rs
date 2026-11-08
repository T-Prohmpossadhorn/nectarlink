// SPDX-License-Identifier: GPL-3.0-or-later
//! Playing streamed 16-bit PCM on this Mac's speakers (or another output) with
//! an AudioToolbox audio queue. The declarations follow the macOS SDK headers
//! `AudioToolbox/AudioQueue.h`, `CoreAudioTypes/CoreAudioBaseTypes.h` and
//! `CoreFoundation/CFString.h`. Playing sound needs no permission.
//!
//! The sound goes into one ring buffer that is made once, before the queue
//! starts. The producer copies each packet in under a short lock. The queue's
//! callback runs on a thread of the audio queue (the run loop is NULL). It takes
//! the same lock only with `try_lock`, copies bytes out and fills the rest with
//! silence. It never allocates or frees memory, and it never logs. Playback
//! starts once a few frames are in, falls back to silence (and waits again) when
//! the network stalls, and drops the oldest sound when it piles up. The ring
//! holds at most [`MOST_MS`] of sound, so that is the most that waits before
//! the queue's own buffers and the device.

#![allow(unsafe_code)]

use std::{
    ffi::{CString, c_char, c_void},
    io::{Error, ErrorKind, Result},
    mem::{align_of, offset_of, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
    sync::{
        Arc, Mutex, PoisonError, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
};

use super::audio::fourcc;

type OSStatus = i32;
type AudioQueueRef = *mut c_void;
type CFTypeRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// `kAudioFormatLinearPCM`.
const FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
/// `kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked`. No big-endian
/// flag: arm64 and x86_64 Macs are little-endian, like the stream.
const FORMAT_FLAGS: u32 = 0x4 | 0x8;
/// `kAudioQueueProperty_CurrentDevice`: the UID of the output, a `CFStringRef`.
const PROPERTY_CURRENT_DEVICE: u32 = fourcc(b"aqcd");

/// The stream's sample rates and channel counts this player accepts. Anything
/// else is refused rather than bent, because a wrong format plays as noise.
const RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
const CHANNELS: std::ops::RangeInclusive<u16> = 1..=8;

/// The longest device UID this player accepts, in bytes. Real UIDs are much
/// shorter. A longer one is treated as a missing device.
const MAX_UID_BYTES: usize = 512;

/// Buffers the queue holds, and how much sound each one has.
const BUFFERS: usize = 3;
const BUFFER_MS: u32 = 20;
/// How much sound is gathered before playing (and again after a stall).
const START_MS: usize = 40;
/// The most sound that waits, in all. A packet that would push the total past
/// this makes the oldest sound go, down to [`TRIM_TO_MS`].
const MOST_MS: usize = 200;
const TRIM_TO_MS: usize = 60;

/// `AudioStreamBasicDescription`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// `AudioQueueBuffer`. The queue owns the memory; the fields are not changed
/// except the byte size.
#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

// The layouts must match the C headers, or the system reads wrong bytes. These
// fail the build, not a test run, if a field is changed by mistake. The sizes
// are those of the LP64 targets (arm64 and x86_64): `UInt32` fields that follow
// a pointer are padded to 8 bytes.
const _: () = {
    assert!(size_of::<StreamDescription>() == 40);
    assert!(align_of::<StreamDescription>() == 8);
    assert!(offset_of!(StreamDescription, format_id) == 8);
    assert!(offset_of!(StreamDescription, format_flags) == 12);
    assert!(offset_of!(StreamDescription, bytes_per_packet) == 16);
    assert!(offset_of!(StreamDescription, frames_per_packet) == 20);
    assert!(offset_of!(StreamDescription, bytes_per_frame) == 24);
    assert!(offset_of!(StreamDescription, channels_per_frame) == 28);
    assert!(offset_of!(StreamDescription, bits_per_channel) == 32);
    assert!(offset_of!(StreamDescription, reserved) == 36);
    assert!(size_of::<QueueBuffer>() == 56);
    assert!(align_of::<QueueBuffer>() == 8);
    assert!(offset_of!(QueueBuffer, data) == 8);
    assert!(offset_of!(QueueBuffer, byte_size) == 16);
    assert!(offset_of!(QueueBuffer, user_data) == 24);
    assert!(offset_of!(QueueBuffer, packet_description_capacity) == 32);
    assert!(offset_of!(QueueBuffer, packet_descriptions) == 40);
    assert!(offset_of!(QueueBuffer, packet_description_count) == 48);
    assert!(size_of::<CFTypeRef>() == 8);
};

type OutputCallback = unsafe extern "C" fn(user: *mut c_void, queue: AudioQueueRef, buffer: *mut QueueBuffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const StreamDescription,
        callback: Option<OutputCallback>,
        user: *mut c_void,
        run_loop: CFTypeRef,
        run_loop_mode: CFTypeRef,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> OSStatus;
    fn AudioQueueAllocateBuffer(queue: AudioQueueRef, size: u32, buffer: *mut *mut QueueBuffer) -> OSStatus;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: *mut QueueBuffer,
        packet_count: u32,
        packets: *const c_void,
    ) -> OSStatus;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> OSStatus;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> OSStatus;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> OSStatus;
    fn AudioQueueSetProperty(queue: AudioQueueRef, property: u32, data: *const c_void, size: u32)
    -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, c_str: *const c_char, encoding: u32) -> CFTypeRef;
}

/// The stream format of `channels` interleaved 16-bit samples at `rate`.
/// `InvalidInput` for a rate or channel count outside [`RATES`] and
/// [`CHANNELS`] (nothing is clamped: that would change how the bytes are read).
fn stream_description(rate: u32, channels: u16) -> Result<StreamDescription> {
    if !RATES.contains(&rate) || !CHANNELS.contains(&channels) {
        return Err(Error::new(ErrorKind::InvalidInput, "unsupported audio format"));
    }
    let frame = 2 * u32::from(channels);
    Ok(StreamDescription {
        sample_rate: f64::from(rate),
        format_id: FORMAT_LINEAR_PCM,
        format_flags: FORMAT_FLAGS,
        bytes_per_packet: frame,
        frames_per_packet: 1,
        bytes_per_frame: frame,
        channels_per_frame: u32::from(channels),
        bits_per_channel: 16,
        reserved: 0,
    })
}

/// Bytes in one queue buffer: whole frames of [`BUFFER_MS`].
fn buffer_bytes(rate: u32, channels: u16) -> u32 {
    rate * BUFFER_MS / 1000 * 2 * u32::from(channels)
}

/// Bytes of `ms` milliseconds of sound, in whole frames (rounded down).
fn ms_bytes(rate: u32, channels: u16, ms: usize) -> usize {
    rate as usize * ms / 1000 * 2 * usize::from(channels)
}

/// How many bytes to drop from the front of `ring ++ incoming` so that the
/// sound that waits stays within `cap` bytes. Returns `(from_ring, from_incoming)`.
///
/// Nothing is dropped while the total fits in `cap`. Above it, the oldest
/// sound goes until at most `keep` bytes are left (`keep` is at most `cap`).
/// Only whole frames of `frame` bytes are dropped, so the ring (which always
/// starts on a frame) keeps the channels in order. A packet bigger than the
/// ring loses its own first bytes after the whole ring is dropped.
fn trim_plan(ring: usize, incoming: usize, frame: usize, cap: usize, keep: usize) -> (usize, usize) {
    let total = ring + incoming;
    if total <= cap {
        return (0, 0);
    }
    let excess = total - keep.min(cap);
    let drop = (excess.div_ceil(frame) * frame).min(total / frame * frame);
    let from_ring = drop.min(ring);
    (from_ring, drop - from_ring)
}

/// The sound waiting to be played: a ring of bytes of interleaved 16-bit
/// samples, with a fixed size that is the whole budget of [`MOST_MS`].
#[derive(Debug)]
struct Ring {
    buf: Box<[u8]>,
    /// Where the oldest byte is.
    head: usize,
    /// Bytes waiting.
    len: usize,
    /// Bytes in one sample frame.
    frame: usize,
    /// Bytes to gather before playing.
    start: usize,
    /// Bytes to keep after a trim.
    keep: usize,
    /// Gathering sound before playing.
    waiting: bool,
    /// Times the sound ran out while playing. A count, nothing else.
    stalls: u64,
    /// Times sound was dropped because too much waited. A count, nothing else.
    trims: u64,
}

impl Ring {
    /// Makes the ring (the only allocation of the player's sound path).
    fn new(rate: u32, channels: u16) -> Ring {
        let frame = 2 * usize::from(channels);
        // The budget is whole frames, so it holds no more than MOST_MS.
        let cap = ms_bytes(rate, channels, MOST_MS).max(frame);
        Ring {
            buf: vec![0; cap].into_boxed_slice(),
            head: 0,
            len: 0,
            frame,
            start: ms_bytes(rate, channels, START_MS),
            keep: ms_bytes(rate, channels, TRIM_TO_MS).clamp(frame, cap),
            waiting: true,
            stalls: 0,
            trims: 0,
        }
    }

    /// Adds sound. A packet may end inside a frame: the next one finishes it,
    /// so every byte is kept. Only whole frames leave ([`Ring::fill`]) or are
    /// dropped from the front, so the channels never swap. The total never
    /// goes past the size of the ring.
    fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        let (from_ring, from_data) = trim_plan(self.len, data.len(), self.frame, cap, self.keep);
        if from_ring + from_data > 0 {
            self.trims += 1;
        }
        self.head = (self.head + from_ring) % cap;
        self.len -= from_ring;
        let data = &data[from_data..];
        // `trim_plan` leaves room for all of `data`.
        let tail = (self.head + self.len) % cap;
        let first = data.len().min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.len += data.len();
    }

    /// Fills `out` (whole frames) with the oldest sound, and silence for what
    /// is missing. When the sound runs out it waits to gather some again.
    /// This does not allocate.
    fn fill(&mut self, out: &mut [u8]) {
        if self.waiting {
            if self.len < self.start {
                out.fill(0);
                return;
            }
            self.waiting = false;
        }
        let take = self.len.min(out.len()) / self.frame * self.frame;
        let cap = self.buf.len();
        let first = take.min(cap - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..take].copy_from_slice(&self.buf[..take - first]);
        self.head = (self.head + take) % cap;
        self.len -= take;
        out[take..].fill(0);
        if take < out.len() {
            if !self.waiting {
                self.stalls += 1;
            }
            self.waiting = true;
        }
    }
}

/// Takes the lock for the callback: never waits. If the producer holds the
/// lock, `out` is silence for this buffer. A panic must not cross the C frame.
fn render(ring: &Mutex<Ring>, out: &mut [u8]) {
    out.fill(0);
    let _ = catch_unwind(AssertUnwindSafe(|| match ring.try_lock() {
        Ok(mut ring) => ring.fill(out),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().fill(out),
        Err(TryLockError::WouldBlock) => {}
    }));
}

/// The producer's side: a short lock, one copy.
fn deliver(ring: &Mutex<Ring>, data: &[u8]) {
    ring.lock().unwrap_or_else(PoisonError::into_inner).push(data);
}

/// True the first time it is called for `flag`, false after that.
fn first_time(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// Says once, at debug level, that the default output is used. Never the UID.
fn note_fallback(reason: &str, status: OSStatus) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if first_time(&LOGGED) {
        tracing::debug!(reason, status, "the chosen output was not used; playing on the default output");
    }
}

/// A UID this player will pass to the system: not empty, not longer than
/// [`MAX_UID_BYTES`] and without a NUL byte.
fn uid_is_usable(uid: &str) -> bool {
    !uid.is_empty() && uid.len() <= MAX_UID_BYTES && !uid.contains('\0')
}

/// Plays sound until dropped.
pub struct Player {
    queue: AudioQueueRef,
    /// The sound. The queue's callback reads it through a raw pointer to this
    /// allocation, so it must outlive the queue: fields drop after
    /// [`Player::drop`] has disposed the queue.
    ring: Arc<Mutex<Ring>>,
    /// The UID string of the chosen output, kept until the queue is disposed.
    /// `AudioQueue.h` only says that `kAudioQueueProperty_CurrentDevice` is a
    /// "read/write property whose value is a CFStringRef that contains the
    /// unique identifier (UID) of the associated audio device". It does not say
    /// that the queue retains the string, so the player keeps its own
    /// reference alive for the whole life of the queue.
    uid: Option<Owned>,
}

// SAFETY: the queue reference is only used to stop and dispose the queue, once,
// from `drop`, and audio queues are thread-safe. The ring is only reached
// through its mutex. The UID string is only released in `drop`.
unsafe impl Send for Player {}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player").finish_non_exhaustive()
    }
}

impl Player {
    /// Starts playing `channels` × 16-bit PCM at `rate` on the default
    /// output device.
    pub fn start(rate: u32, channels: u16) -> Result<Player> {
        Self::start_on(None, rate, channels)
    }

    /// As [`Player::start`], on the output device with this UID (from
    /// [`super::audio::output_devices`]). The default output is used when it
    /// is `None` or the device does not take it (gone, for instance).
    pub fn start_on(device: Option<String>, rate: u32, channels: u16) -> Result<Player> {
        let format = stream_description(rate, channels)?;
        let ring = Arc::new(Mutex::new(Ring::new(rate, channels)));
        // Take the lock once here, so that any first-use work of the mutex
        // happens on this thread and not in the audio callback.
        drop(ring.lock());
        let mut queue: AudioQueueRef = ptr::null_mut();
        // SAFETY: `format` is valid for the call and copied; the ring's
        // allocation stays valid until the queue is disposed (see `Player`). A
        // NULL run loop means the callback runs on a thread of the queue.
        let status = unsafe {
            AudioQueueNewOutput(
                &raw const format,
                Some(on_buffer),
                Arc::as_ptr(&ring).cast_mut().cast(),
                ptr::null(),
                ptr::null(),
                0,
                &raw mut queue,
            )
        };
        if status != 0 || queue.is_null() {
            return Err(failure("the audio queue was not created", status));
        }
        let mut player = Player { queue, ring, uid: None };
        if let Some(uid) = device {
            player.choose(&uid);
        }
        player.prime(buffer_bytes(rate, channels))?;
        // SAFETY: the queue is valid and owned by `player`.
        let status = unsafe { AudioQueueStart(queue, ptr::null()) };
        if status != 0 {
            return Err(failure("the audio queue did not start", status));
        }
        Ok(player)
    }

    /// Plays on the output with this UID. If that fails, the queue stays on
    /// the default output. The UID is never logged.
    fn choose(&mut self, uid: &str) {
        if !uid_is_usable(uid) {
            note_fallback("the device id is not usable", 0);
            return;
        }
        let Ok(uid) = CString::new(uid) else {
            note_fallback("the device id is not usable", 0);
            return;
        };
        // SAFETY: `uid` is a valid NUL-terminated string for the call.
        let name = unsafe { CFStringCreateWithCString(ptr::null(), uid.as_ptr(), CF_STRING_ENCODING_UTF8) };
        if name.is_null() {
            note_fallback("the device id could not be made into a string", 0);
            return;
        }
        // The player owns the string until the queue is disposed.
        let name = &self.uid.insert(Owned(name)).0;
        // SAFETY: the property takes a pointer to a `CFStringRef` and its size.
        // The pointer is only read during the call; the string itself stays
        // alive in `self.uid` as long as the queue exists.
        let status = unsafe {
            AudioQueueSetProperty(
                self.queue,
                PROPERTY_CURRENT_DEVICE,
                ptr::from_ref(name).cast(),
                size_of::<CFTypeRef>() as u32,
            )
        };
        if status != 0 {
            note_fallback("the output was refused", status);
        }
    }

    /// Gives the queue its silent buffers, so it has something to play at start.
    fn prime(&self, bytes: u32) -> Result<()> {
        for _ in 0..BUFFERS {
            let mut buffer: *mut QueueBuffer = ptr::null_mut();
            // SAFETY: the queue is valid; `buffer` receives a buffer the queue owns.
            let status = unsafe { AudioQueueAllocateBuffer(self.queue, bytes, &raw mut buffer) };
            if status != 0 || buffer.is_null() {
                return Err(failure("an audio buffer was not allocated", status));
            }
            // SAFETY: `buffer` is valid and not yet enqueued, so only this thread uses it.
            let status = unsafe { enqueue(self.queue, buffer, Arc::as_ptr(&self.ring), false) };
            if status != 0 {
                return Err(failure("an audio buffer was not queued", status));
            }
        }
        Ok(())
    }

    /// Hands over a packet. It is copied into the ring and the `Vec` is freed
    /// here, never in the callback. When more than the budget would wait, the
    /// oldest sound is dropped (it would be too late to hear anyway).
    pub fn push(&self, data: Vec<u8>) {
        deliver(&self.ring, &data);
        drop(data);
    }
}

/// An owned (Create rule) CF object, released on drop.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the guard owns one non-null reference and releases it once.
        unsafe { CFRelease(self.0) };
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: both calls are synchronous, so no callback runs after the
        // dispose returns. This runs on a thread that is not a callback of
        // this queue. The ring and the UID string are freed after this
        // function returns, so after the queue is gone.
        unsafe {
            AudioQueueStop(self.queue, 1);
            AudioQueueDispose(self.queue, 1);
        }
        let ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        // Counts only, never what was played.
        tracing::debug!(stalls = ring.stalls, trims = ring.trims, "audio playback ended");
    }
}

/// An error for a failed call. Only the status code, never what was playing.
fn failure(what: &str, status: OSStatus) -> Error {
    Error::other(format!("{what} (OSStatus {status})"))
}

/// Fills `buffer` from `ring` (silence if the ring is busy, or when `fill`
/// is false) and queues it. This does not allocate or free memory.
///
/// # Safety
/// `queue` and `buffer` are valid, and `ring` points to the mutex made in
/// `start_on`, not yet freed. No other thread uses `buffer`.
unsafe fn enqueue(
    queue: AudioQueueRef,
    buffer: *mut QueueBuffer,
    ring: *const Mutex<Ring>,
    fill: bool,
) -> OSStatus {
    // SAFETY: from the contract above; the queue's memory holds `capacity` bytes.
    let (out, size) = unsafe {
        let b = &mut *buffer;
        (slice::from_raw_parts_mut(b.data.cast::<u8>(), b.capacity as usize), b.capacity)
    };
    out.fill(0);
    if fill {
        // SAFETY: `ring` is valid until the queue is disposed.
        render(unsafe { &*ring }, out);
    }
    // SAFETY: `buffer` is valid and its size is set to the whole buffer.
    unsafe {
        (*buffer).byte_size = size;
        AudioQueueEnqueueBuffer(queue, buffer, 0, ptr::null())
    }
}

/// The queue is done with `buffer`: fill it again and queue it. It fails
/// harmlessly while the queue stops.
unsafe extern "C" fn on_buffer(user: *mut c_void, queue: AudioQueueRef, buffer: *mut QueueBuffer) {
    // SAFETY: `user` is the ring from `start_on`; it is freed only after the
    // queue is disposed, and no callback runs after that.
    unsafe { enqueue(queue, buffer, user.cast::<Mutex<Ring>>(), true) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_format_is_interleaved_s16_little_endian() {
        let d = stream_description(48_000, 2).unwrap();
        assert_eq!(d.sample_rate, 48_000.0);
        assert_eq!(d.format_id, 0x6C70_636D);
        assert_eq!(d.format_flags, 12, "signed integer, packed, not big-endian");
        assert_eq!((d.bytes_per_packet, d.frames_per_packet, d.bytes_per_frame), (4, 1, 4));
        assert_eq!((d.channels_per_frame, d.bits_per_channel, d.reserved), (2, 16, 0));
        let mono = stream_description(16_000, 1).unwrap();
        assert_eq!((mono.bytes_per_packet, mono.bytes_per_frame, mono.channels_per_frame), (2, 2, 1));
    }

    #[test]
    fn the_property_id_matches_the_header() {
        // The struct layouts are checked at compile time (see the asserts above).
        assert_eq!(PROPERTY_CURRENT_DEVICE, 0x6171_6364);
    }

    #[test]
    fn formats_out_of_range_are_refused_not_bent() {
        for (rate, channels) in [(0, 2), (7_999, 2), (192_001, 2), (48_000, 0), (48_000, 9)] {
            let e = stream_description(rate, channels).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidInput, "{rate} {channels}");
        }
        assert!(stream_description(8_000, 8).is_ok());
        assert!(stream_description(192_000, 1).is_ok());
        // The player refuses before it calls the system.
        assert_eq!(Player::start(0, 2).unwrap_err().kind(), ErrorKind::InvalidInput);
        assert_eq!(
            Player::start_on(Some("x".into()), 48_000, 0).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_buffer_holds_whole_frames() {
        assert_eq!(buffer_bytes(48_000, 2), 960 * 4);
        assert_eq!(buffer_bytes(44_100, 1), 882 * 2);
        assert_eq!(buffer_bytes(44_100, 2) % 4, 0);
        for rate in [8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000] {
            for channels in 1..=8 {
                assert_eq!(buffer_bytes(rate, channels) % (2 * u32::from(channels)), 0);
                assert!(buffer_bytes(rate, channels) > 0);
            }
        }
    }

    #[test]
    fn the_budget_is_whole_frames_and_never_more_than_the_most() {
        assert_eq!(ms_bytes(48_000, 2, MOST_MS), 9_600 * 4);
        assert_eq!(ms_bytes(1000, 1, MOST_MS), 400);
        for rate in [8_000, 11_025, 22_050, 44_100, 48_000, 88_200, 96_000, 192_000, 12_345] {
            for channels in 1..=8u16 {
                let frame = 2 * usize::from(channels);
                let cap = ms_bytes(rate, channels, MOST_MS);
                assert_eq!(cap % frame, 0, "whole frames");
                // cap / (bytes per second) <= MOST_MS / 1000, in integers.
                assert!(cap * 1000 <= rate as usize * frame * MOST_MS, "{rate} {channels}");
                let ring = Ring::new(rate, channels);
                assert_eq!(ring.buf.len(), cap, "the ring is exactly the budget");
                assert!(ring.keep <= cap && ring.keep >= frame);
                assert_eq!(ring.start % frame, 0);
                assert!(ring.start < ring.keep, "it starts before it would trim");
            }
        }
    }

    #[test]
    fn the_trim_math_keeps_whole_frames_within_the_budget() {
        // Mono 16-bit, 1 kHz: frame 2, cap 400 (200 ms), keep 120 (60 ms).
        let (frame, cap, keep) = (2, 400, 120);
        // Fits: nothing goes.
        assert_eq!(trim_plan(0, 400, frame, cap, keep), (0, 0));
        assert_eq!(trim_plan(300, 100, frame, cap, keep), (0, 0), "an exact fit");
        // One byte over: the oldest go down to the keep amount.
        assert_eq!(trim_plan(300, 101, frame, cap, keep), (282, 0));
        // A packet bigger than the ring: all of the ring, then its own start.
        assert_eq!(trim_plan(100, 600, frame, cap, keep), (100, 480));
        assert_eq!(trim_plan(0, 600, frame, cap, keep), (0, 480));
        // An odd tail (a split frame) still drops whole frames only.
        let (from_ring, from_data) = trim_plan(301, 100, frame, cap, keep);
        assert_eq!((from_ring + from_data) % frame, 0);
        assert!(301 + 100 - from_ring - from_data <= keep);
        // The result always fits, for every size, on 8 channels too.
        for (frame, cap) in [(2usize, 400usize), (16, 6_400), (4, 800)] {
            let keep = cap * 3 / 10 / frame * frame;
            for ring in (0..=cap).step_by(frame / 2) {
                for incoming in [0, 1, 3, 15, 160, cap, cap + 1, 5 * cap + 7] {
                    let (a, b) = trim_plan(ring, incoming, frame, cap, keep);
                    assert!(a <= ring && b <= incoming);
                    assert_eq!((a + b) % frame, 0, "whole frames only");
                    assert!(ring + incoming - a - b <= cap, "never over the budget");
                    if ring + incoming <= cap {
                        assert_eq!((a, b), (0, 0), "nothing is lost when it fits");
                    }
                }
            }
        }
    }

    /// Mono frames numbered from `first`, so a frame is 2 bytes.
    fn frames(first: u16, count: usize) -> Vec<u8> {
        (0..count).flat_map(|i| (first.wrapping_add(i as u16)).to_le_bytes()).collect()
    }

    /// What is waiting in the ring, oldest first.
    fn waiting(ring: &Ring) -> Vec<u8> {
        let cap = ring.buf.len();
        (0..ring.len).map(|i| ring.buf[(ring.head + i) % cap]).collect()
    }

    #[test]
    fn it_waits_for_some_sound_then_plays_it_in_order() {
        // Mono at 1 kHz: one frame is one millisecond.
        let mut ring = Ring::new(1000, 1);
        let mut out = [9u8; 20];
        ring.push(&frames(1, 10));
        ring.fill(&mut out);
        assert_eq!(out, [0; 20], "10 ms is less than the start amount: silence");
        ring.push(&frames(11, 40));
        let mut out = [0u8; 20];
        ring.fill(&mut out);
        assert_eq!(out.to_vec(), frames(1, 10), "50 ms in: it plays the oldest first");
        ring.fill(&mut out);
        assert_eq!(out.to_vec(), frames(11, 10));
        assert_eq!((ring.stalls, ring.trims), (0, 0));
    }

    #[test]
    fn it_fills_with_silence_and_waits_again_when_the_sound_runs_out() {
        let mut ring = Ring::new(1000, 1);
        ring.push(&frames(1, 45));
        let mut out = [0u8; 100];
        ring.fill(&mut out);
        assert_eq!(out[..90].to_vec(), frames(1, 45));
        assert_eq!(out[90..], [0; 10], "the rest is silence");
        assert_eq!(ring.stalls, 1, "one run-out is one stall");
        // It waits for the start amount again, and waiting is not counted again.
        ring.push(&frames(1, 10));
        let mut out = [7u8; 20];
        ring.fill(&mut out);
        assert_eq!(out, [0; 20]);
        ring.fill(&mut out);
        assert_eq!(ring.stalls, 1);
        // An empty ring that never started is silence, and is not a stall.
        let mut fresh = Ring::new(1000, 1);
        let mut out = [3u8; 8];
        fresh.fill(&mut out);
        assert_eq!((out, fresh.stalls), ([0; 8], 0));
    }

    #[test]
    fn silence_fills_the_whole_output_whatever_was_in_it() {
        let mut ring = Ring::new(1000, 2);
        let mut out = [0xFFu8; 64];
        ring.fill(&mut out);
        assert!(out.iter().all(|b| *b == 0));
        let mut empty: [u8; 0] = [];
        ring.fill(&mut empty);
    }

    #[test]
    fn a_frame_split_across_packets_plays_in_the_right_order() {
        // Stereo at 1 kHz: a frame is 4 bytes (left, right).
        let mut ring = Ring::new(1000, 2);
        ring.push(&[1, 0, 2, 0, 3]); // the third frame starts here
        ring.push(&[0, 4, 0, 5, 0, 6, 0]);
        ring.push(&[0u8; 4 * 40]);
        let mut out = [0u8; 12];
        ring.fill(&mut out);
        assert_eq!(out, [1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
        // An output that is not a multiple of a frame is not over-read either.
        let mut odd = [9u8; 6];
        ring.fill(&mut odd);
        assert_eq!(odd, [0; 6], "the sound after it is silence, and only whole frames are copied");
        let mut ring = Ring::new(1000, 2);
        ring.push(&[1, 0, 2, 0, 3, 0, 4, 0]);
        ring.push(&[0u8; 4 * 40]);
        let mut odd = [9u8; 6];
        ring.fill(&mut odd);
        assert_eq!(odd, [1, 0, 2, 0, 0, 0], "only the one whole frame fits");
    }

    #[test]
    fn it_drops_the_oldest_sound_when_it_piles_up() {
        let mut ring = Ring::new(1000, 1);
        // 300 ms at 1 kHz, mono: well over the most it keeps.
        ring.push(&frames(0, 300));
        assert_eq!(ring.len, 2 * TRIM_TO_MS, "down to the trim amount");
        assert_eq!(waiting(&ring), frames(300 - TRIM_TO_MS as u16, TRIM_TO_MS), "the newest sound is kept");
        assert_eq!(ring.trims, 1);
        // Sound that was already waiting goes first, and the new packet is whole.
        let mut ring = Ring::new(1000, 1);
        ring.push(&frames(0, 150));
        assert_eq!(ring.trims, 0);
        ring.push(&frames(150, 100));
        assert_eq!(ring.trims, 1);
        assert!(ring.len <= ring.buf.len());
        let kept = waiting(&ring);
        assert_eq!(kept, frames(250 - (kept.len() / 2) as u16, kept.len() / 2));
    }

    #[test]
    fn the_total_never_passes_the_budget_and_frames_stay_in_step() {
        // Stereo at 1 kHz. A frame is [n, 0xAA, n, 0xBB], so a shifted read shows.
        let mut ring = Ring::new(1000, 2);
        let cap = ring.buf.len();
        let stream: Vec<u8> = (0..4000usize).flat_map(|n| [n as u8, 0xAA, n as u8, 0xBB]).collect();
        let mut at = 0;
        for size in [7, 13, 1000, 3, 600, 1, 5, 2000, 81, 4, 999].into_iter().cycle().take(40) {
            let end = (at + size).min(stream.len());
            ring.push(&stream[at..end]);
            at = if end == stream.len() { 0 } else { end };
            assert!(ring.len <= cap, "never more than the budget");
            let bytes = waiting(&ring);
            for frame in bytes.as_chunks::<4>().0 {
                assert_eq!((frame[1], frame[3], frame[0]), (0xAA, 0xBB, frame[2]), "channels in step");
            }
        }
        assert!(ring.trims > 0);
        assert_eq!(ring.buf.len(), cap, "the ring never changes size");
    }

    #[test]
    fn the_ring_keeps_the_order_when_it_wraps() {
        let mut ring = Ring::new(1000, 1);
        ring.push(&frames(0, 50));
        let mut expect = 0u16;
        let mut out = [0u8; 60];
        ring.fill(&mut out);
        assert_eq!(out.to_vec(), frames(0, 30));
        expect += 30;
        // 100 rounds of 30 frames in and out: the head goes round the 200-frame ring.
        let mut next = 50u16;
        for _ in 0..100 {
            ring.push(&frames(next, 30));
            next += 30;
            ring.fill(&mut out);
            assert_eq!(out.to_vec(), frames(expect, 30));
            expect += 30;
        }
        assert_eq!((ring.stalls, ring.trims), (0, 0));
        assert!(ring.head < ring.buf.len());
    }

    #[test]
    fn the_callback_fills_the_buffer_and_never_waits_for_the_lock() {
        let ring = Mutex::new(Ring::new(1000, 1));
        let mut out = [5u8; 10];
        render(&ring, &mut out);
        assert_eq!(out, [0; 10], "nothing yet: silence");
        deliver(&ring, &frames(1, 60));
        let mut out = [0u8; 10];
        render(&ring, &mut out);
        assert_eq!(out.to_vec(), frames(1, 5));
        // The producer holds the lock: the callback gives silence and does not block.
        {
            let _guard = ring.lock().unwrap();
            let mut out = [7u8; 10];
            render(&ring, &mut out);
            assert_eq!(out, [0; 10]);
        }
        render(&ring, &mut out);
        assert_eq!(out.to_vec(), frames(6, 5), "no sound was lost by the busy callback");
    }

    #[test]
    fn a_poisoned_lock_still_plays() {
        let ring = Arc::new(Mutex::new(Ring::new(1000, 1)));
        deliver(&ring, &frames(1, 60));
        let other = Arc::clone(&ring);
        let _ = std::thread::spawn(move || {
            let _guard = other.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(ring.is_poisoned());
        let mut out = [0u8; 10];
        render(&ring, &mut out);
        assert_eq!(out.to_vec(), frames(1, 5));
        deliver(&ring, &frames(61, 1));
    }

    #[test]
    fn a_device_id_must_be_short_and_without_nul() {
        assert!(uid_is_usable("BuiltInSpeakerDevice"));
        assert!(uid_is_usable(&"a".repeat(MAX_UID_BYTES)));
        assert!(!uid_is_usable(&"a".repeat(MAX_UID_BYTES + 1)));
        assert!(!uid_is_usable(""));
        assert!(!uid_is_usable("a\0b"));
    }

    #[test]
    fn the_fallback_is_noted_only_once() {
        let flag = AtomicBool::new(false);
        assert!(first_time(&flag));
        assert!(!first_time(&flag));
        assert!(!first_time(&flag));
    }
}
