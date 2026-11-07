// SPDX-License-Identifier: GPL-3.0-or-later
//! Playing streamed 16-bit PCM on this Mac's speakers (or another output) with
//! an AudioToolbox audio queue. The declarations follow the macOS SDK headers
//! `AudioToolbox/AudioQueue.h`, `CoreAudioTypes/CoreAudioBaseTypes.h` and
//! `CoreFoundation/CFString.h`. Playing sound needs no permission.
//!
//! Packets go through a small bounded channel to the queue's callback. The
//! callback runs on a thread of the audio queue (the run loop is NULL), so it
//! only moves bytes: it favors low delay like the Windows player. Playback
//! starts once a few packets are in, falls back to silence (and waits again)
//! when the network stalls, and drops the oldest sound when it piles up.

#![allow(unsafe_code)]

use std::{
    collections::VecDeque,
    ffi::{CString, c_char, c_void},
    io::{Error, ErrorKind, Result},
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
    sync::{
        Mutex, TryLockError,
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
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

/// Buffers the queue holds, and how much sound each one has.
const BUFFERS: usize = 3;
const BUFFER_MS: u32 = 20;
/// Sound packets waiting for the callback, at most.
const QUEUE: usize = 64;
/// How much sound is gathered before playing (and again after a stall).
const START_MS: usize = 40;
/// More than this waiting, and the oldest is dropped down to [`TRIM_TO_MS`].
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

/// The jitter buffer, in bytes of interleaved 16-bit samples.
#[derive(Debug)]
struct Jitter {
    bytes: VecDeque<u8>,
    /// Bytes in one sample frame.
    frame: usize,
    /// Bytes in one millisecond.
    per_ms: usize,
    /// Gathering sound before playing.
    waiting: bool,
}

impl Jitter {
    fn new(rate: u32, channels: u16) -> Jitter {
        let frame = 2 * usize::from(channels);
        Jitter { bytes: VecDeque::new(), frame, per_ms: frame * rate as usize / 1000, waiting: true }
    }

    /// Adds sound. A packet may end inside a frame: the next one finishes it,
    /// so every byte is kept. Only whole frames leave ([`Jitter::fill`]) or are
    /// dropped from the front, so the channels never swap.
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        if self.bytes.len() > self.per_ms * MOST_MS {
            let keep = self.per_ms * TRIM_TO_MS;
            let drop = (self.bytes.len() - keep) / self.frame * self.frame;
            self.bytes.drain(..drop);
        }
    }

    /// Fills `out` (whole frames) with the oldest sound, and silence for what
    /// is missing. When the sound runs out it waits to gather some again.
    fn fill(&mut self, out: &mut [u8]) {
        if self.waiting {
            if self.bytes.len() < self.per_ms * START_MS {
                out.fill(0);
                return;
            }
            self.waiting = false;
        }
        let take = self.bytes.len().min(out.len()) / self.frame * self.frame;
        for (slot, byte) in out.iter_mut().zip(self.bytes.drain(..take)) {
            *slot = byte;
        }
        out[take..].fill(0);
        if take < out.len() {
            self.waiting = true;
        }
    }
}

/// What the queue's callback works on.
struct State {
    packets: Receiver<Vec<u8>>,
    jitter: Jitter,
}

impl State {
    /// Takes in what has arrived, then fills `out` for the speakers.
    fn render(&mut self, out: &mut [u8]) {
        while let Ok(packet) = self.packets.try_recv() {
            self.jitter.push(&packet);
        }
        self.jitter.fill(out);
    }
}

/// Plays sound until dropped.
pub struct Player {
    queue: AudioQueueRef,
    /// Owned by the player; freed after the queue is disposed.
    state: *mut Mutex<State>,
    packets: SyncSender<Vec<u8>>,
}

// SAFETY: the queue reference is only used to stop and dispose the queue, once,
// from `drop`, and audio queues are thread-safe. `state` is only reached
// through its mutex (by the callback) and freed in `drop`.
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
        let (packets, receiver) = sync_channel(QUEUE);
        let state = Box::into_raw(Box::new(Mutex::new(State {
            packets: receiver,
            jitter: Jitter::new(rate, channels),
        })));
        let mut queue: AudioQueueRef = ptr::null_mut();
        // SAFETY: `format` is valid for the call and copied; `state` stays valid
        // until the queue is disposed. A NULL run loop means the callback runs
        // on a thread of the queue.
        let status = unsafe {
            AudioQueueNewOutput(
                &raw const format,
                Some(on_buffer),
                state.cast(),
                ptr::null(),
                ptr::null(),
                0,
                &raw mut queue,
            )
        };
        if status != 0 || queue.is_null() {
            // SAFETY: no queue exists, so no callback can run and `state` is unique.
            drop(unsafe { Box::from_raw(state) });
            return Err(failure("the audio queue was not created", status));
        }
        let player = Player { queue, state, packets };
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
    fn choose(&self, uid: &str) {
        let Ok(uid) = CString::new(uid) else { return };
        // SAFETY: `uid` is a valid NUL-terminated string for the call.
        let name = unsafe { CFStringCreateWithCString(ptr::null(), uid.as_ptr(), CF_STRING_ENCODING_UTF8) };
        if name.is_null() {
            return;
        }
        // SAFETY: the property takes a pointer to a `CFStringRef` and its size.
        // The queue copies the string, so it is released right after.
        let status = unsafe {
            let status = AudioQueueSetProperty(
                self.queue,
                PROPERTY_CURRENT_DEVICE,
                (&raw const name).cast(),
                size_of::<CFTypeRef>() as u32,
            );
            CFRelease(name);
            status
        };
        if status != 0 {
            tracing::warn!(status, "the chosen output was refused; playing on the default output");
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
            let status = unsafe { enqueue(self.queue, buffer, self.state, false) };
            if status != 0 {
                return Err(failure("an audio buffer was not queued", status));
            }
        }
        Ok(())
    }

    /// Hands over a packet without waiting (dropped if the player is far
    /// behind: it would be too late to hear anyway).
    pub fn push(&self, data: Vec<u8>) {
        if let Err(TrySendError::Full(_)) = self.packets.try_send(data) {
            tracing::debug!("a sound packet was dropped");
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: both calls are synchronous, so no callback runs after the
        // dispose returns. Only then is `state` freed, once. This runs on a
        // thread that is not a callback of this queue.
        unsafe {
            AudioQueueStop(self.queue, 1);
            AudioQueueDispose(self.queue, 1);
            drop(Box::from_raw(self.state));
        }
    }
}

/// An error for a failed call. Only the status code, never what was playing.
fn failure(what: &str, status: OSStatus) -> Error {
    Error::other(format!("{what} (OSStatus {status})"))
}

/// Fills `buffer` from `state` (silence if the state is busy, or when `fill`
/// is false) and queues it.
///
/// # Safety
/// `queue` and `buffer` are valid, and `state` is the pointer made in
/// `start_on`, not yet freed. No other thread uses `buffer`.
unsafe fn enqueue(
    queue: AudioQueueRef,
    buffer: *mut QueueBuffer,
    state: *mut Mutex<State>,
    fill: bool,
) -> OSStatus {
    // SAFETY: from the contract above; the queue's memory holds `capacity` bytes.
    let (out, size) = unsafe {
        let b = &mut *buffer;
        (slice::from_raw_parts_mut(b.data.cast::<u8>(), b.capacity as usize), b.capacity)
    };
    out.fill(0);
    if fill {
        // A panic here must not cross the C frame; the buffer stays silent.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `state` is valid until the queue is disposed.
            let state = unsafe { &*state };
            match state.try_lock() {
                Ok(mut state) => state.render(out),
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().render(out),
                Err(TryLockError::WouldBlock) => {}
            }
        }));
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
    // SAFETY: `user` is the state from `start_on`; it is freed only after the
    // queue is disposed, and no callback runs after that.
    unsafe { enqueue(queue, buffer, user.cast(), true) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

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
    fn the_stream_format_matches_the_header_layout() {
        assert_eq!(size_of::<StreamDescription>(), 40);
        assert_eq!(std::mem::align_of::<StreamDescription>(), 8);
        // `AudioQueueBuffer`: 3 pointers and 4 `UInt32`s, each `UInt32` padded to 8 bytes.
        assert_eq!(size_of::<QueueBuffer>(), 56);
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

    /// 1 kHz of frames numbered from `first`, mono, so a frame is 2 bytes.
    fn frames(first: u16, count: usize) -> Vec<u8> {
        (0..count).flat_map(|i| (first + i as u16).to_le_bytes()).collect()
    }

    #[test]
    fn it_waits_for_some_sound_then_plays_it_in_order() {
        // Mono at 1 kHz: one frame is one millisecond.
        let mut jitter = Jitter::new(1000, 1);
        let mut out = [9u8; 20];
        jitter.push(&frames(1, 10));
        jitter.fill(&mut out);
        assert_eq!(out, [0; 20], "10 ms is less than the start amount: silence");
        jitter.push(&frames(11, 40));
        let mut out = [0u8; 20];
        jitter.fill(&mut out);
        assert_eq!(out.to_vec(), frames(1, 10), "50 ms in: it plays the oldest first");
        jitter.fill(&mut out);
        assert_eq!(out.to_vec(), frames(11, 10));
    }

    #[test]
    fn it_fills_with_silence_and_waits_again_when_the_sound_runs_out() {
        let mut jitter = Jitter::new(1000, 1);
        jitter.push(&frames(1, 45));
        let mut out = [0u8; 100];
        jitter.fill(&mut out);
        assert_eq!(out[..90].to_vec(), frames(1, 45));
        assert_eq!(out[90..], [0; 10], "the rest is silence");
        // It waits for the start amount again.
        jitter.push(&frames(1, 10));
        let mut out = [7u8; 20];
        jitter.fill(&mut out);
        assert_eq!(out, [0; 20]);
    }

    #[test]
    fn a_frame_split_across_packets_plays_in_the_right_order() {
        // Stereo at 1 kHz: a frame is 4 bytes (left, right).
        let mut jitter = Jitter::new(1000, 2);
        jitter.push(&[1, 0, 2, 0, 3]); // the third frame starts here
        jitter.push(&[0, 4, 0, 5, 0, 6, 0]);
        jitter.push(&[0u8; 4 * 40]);
        let mut out = [0u8; 12];
        jitter.fill(&mut out);
        assert_eq!(out, [1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
        // An output that is not a multiple of a frame is not over-read either.
        let mut odd = [9u8; 6];
        jitter.fill(&mut odd);
        assert_eq!(odd, [0; 6], "the sound after it is silence, and only whole frames are copied");
        let mut jitter = Jitter::new(1000, 2);
        jitter.push(&[1, 0, 2, 0, 3, 0, 4, 0]);
        jitter.push(&[0u8; 4 * 40]);
        let mut odd = [9u8; 6];
        jitter.fill(&mut odd);
        assert_eq!(odd, [1, 0, 2, 0, 0, 0], "only the one whole frame fits");
    }

    #[test]
    fn it_drops_the_oldest_sound_when_it_piles_up() {
        let mut jitter = Jitter::new(1000, 1);
        // 300 ms at 1 kHz, mono: well over the most it keeps.
        jitter.push(&frames(0, 300));
        assert_eq!(jitter.bytes.len(), 2 * TRIM_TO_MS, "down to the trim amount");
        let newest: Vec<u8> = jitter.bytes.iter().copied().collect();
        assert_eq!(newest, frames(300 - TRIM_TO_MS as u16, TRIM_TO_MS), "the newest sound is kept");
    }

    #[test]
    fn the_callback_state_takes_what_arrived_and_fills_the_buffer() {
        let (tx, rx) = sync_channel(QUEUE);
        let mut state = State { packets: rx, jitter: Jitter::new(1000, 1) };
        let mut out = [5u8; 10];
        state.render(&mut out);
        assert_eq!(out, [0; 10], "nothing yet: silence");
        for first in [1u16, 21, 41] {
            tx.send(frames(first, 20)).unwrap();
        }
        let mut out = [0u8; 10];
        state.render(&mut out);
        assert_eq!(out.to_vec(), frames(1, 5));
        // A full channel drops instead of blocking.
        let (small_tx, _small_rx) = sync_channel::<Vec<u8>>(1);
        small_tx.try_send(vec![1]).unwrap();
        assert!(matches!(small_tx.try_send(vec![2]), Err(TrySendError::Full(_))));
    }
}
