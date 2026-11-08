// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS MP4 recording stubs.

use std::{
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Whether this build can record the mirror to an MP4 file. A Mac cannot yet: this
/// file is a stub, so the window hides its record button and Ctrl+R instead of
/// showing a recording that never saves.
pub fn supported() -> bool {
    false
}

/// `"{prefix} YYYY-MM-DD HH.MM.SS.{ext}"` in local time, the same shape as on Windows.
pub fn local_timestamp_filename(prefix: &str, ext: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let secs = libc::time_t::try_from(now).unwrap_or(0);
    // SAFETY: `tm` is plain data that `localtime_r` fills in; it takes no other state.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return format!("{prefix} {now}.{ext}");
        }
        tm
    };
    format!(
        "{prefix} {:04}-{:02}-{:02} {:02}.{:02}.{:02}.{ext}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub struct MirrorRecorder {
    path: PathBuf,
    started_at: Instant,
    started_epoch_ms: i64,
}

impl std::fmt::Debug for MirrorRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorRecorder").field("path", &self.path).finish_non_exhaustive()
    }
}

impl MirrorRecorder {
    pub fn new(
        path: PathBuf,
        _width: u32,
        _height: u32,
        _fps: u32,
        _audio_cfg: Option<(u32, u8)>,
    ) -> MirrorRecorder {
        let started_epoch_ms =
            SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        MirrorRecorder { path, started_at: Instant::now(), started_epoch_ms }
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn started_epoch_ms(&self) -> i64 {
        self.started_epoch_ms
    }

    pub fn set_sps_pps(&mut self, _sps_pps: &[u8]) {}

    pub fn on_config(&mut self, _width: u32, _height: u32) {}

    pub fn on_audio_config(&mut self, _rate: u32, _channels: u8) {}

    pub fn on_packet(&mut self, _keyframe: bool, _time_us: u64, _data: &[u8]) -> Result<(), String> {
        Err("screen recording is not supported on macOS".into())
    }

    pub fn on_audio(&mut self, _time_us: u64, _pcm: &[u8]) -> Result<(), String> {
        Err("audio recording is not supported on macOS".into())
    }

    pub fn finish(self) -> Result<PathBuf, String> {
        Err("screen recording is not supported on macOS".into())
    }
}

/// The NAL units of an Annex-B stream, as (type, payload) pairs.
fn annex_b_nals(data: &[u8]) -> Vec<(u8, &[u8])> {
    // (offset of the start code, offset of the payload) for every start code.
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (idx, &(_, payload_start)) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).map_or(data.len(), |&(next, _)| next);
        if payload_start < end {
            let payload = &data[payload_start..end];
            out.push((payload[0] & 0x1F, payload));
        }
    }
    out
}

/// Keeps the newest SPS (NAL 7) and PPS (NAL 8) from `data` in `sps_pps`, each
/// with a 4-byte `00 00 00 01` start code. At most one of each is kept, so the
/// buffer stays small however many keyframes arrive.
pub fn merge_sps_pps(sps_pps: &mut Vec<u8>, data: &[u8]) {
    let new = annex_b_nals(data);
    if !new.iter().any(|&(t, _)| t == 7 || t == 8) {
        return;
    }
    let find = |nals: &[(u8, &[u8])], kind: u8| {
        nals.iter().rev().find(|&&(t, _)| t == kind).map(|&(_, p)| p.to_vec())
    };
    let old = annex_b_nals(sps_pps);
    let sps = find(&new, 7).or_else(|| find(&old, 7));
    let pps = find(&new, 8).or_else(|| find(&old, 8));
    sps_pps.clear();
    for payload in [sps, pps].into_iter().flatten() {
        sps_pps.extend_from_slice(&[0, 0, 0, 1]);
        sps_pps.extend_from_slice(&payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_cannot_record_yet() {
        assert!(!supported());
    }

    #[test]
    fn mp4_recorder_reports_unsupported() {
        let mut rec = MirrorRecorder::new(PathBuf::from("/tmp/dummy.mp4"), 1920, 1080, 60, None);
        assert!(rec.on_packet(true, 0, &[1, 2, 3]).is_err());
        assert!(rec.on_audio(0, &[1, 2, 3]).is_err());
        assert!(rec.finish().is_err());
    }

    #[test]
    fn local_timestamp_filename_formats_correctly() {
        let name = local_timestamp_filename("Screen", "mp4");
        assert!(name.starts_with("Screen "));
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn local_timestamp_filename_has_the_windows_shape() {
        let name = local_timestamp_filename("Mirror", "mp4");
        let stamp = name.strip_prefix("Mirror ").and_then(|r| r.strip_suffix(".mp4")).unwrap();
        // YYYY-MM-DD HH.MM.SS
        let shape: String = stamp.chars().map(|c| if c.is_ascii_digit() { '9' } else { c }).collect();
        assert_eq!(shape, "9999-99-99 99.99.99");
    }

    #[test]
    fn merge_sps_pps_stays_bounded() {
        let keyframe: Vec<u8> = [
            &[0, 0, 0, 1, 0x67, 1, 2, 3][..],
            &[0, 0, 0, 1, 0x68, 4, 5][..],
            &[0, 0, 0, 1, 0x65, 9, 9, 9, 9][..],
        ]
        .concat();
        let mut held = Vec::new();
        merge_sps_pps(&mut held, &keyframe);
        let first = held.clone();
        assert_eq!(first, [&[0, 0, 0, 1, 0x67, 1, 2, 3][..], &[0, 0, 0, 1, 0x68, 4, 5][..]].concat());
        for _ in 0..1000 {
            merge_sps_pps(&mut held, &keyframe);
        }
        assert_eq!(held, first);
        // A frame with no parameter sets changes nothing.
        merge_sps_pps(&mut held, &[0, 0, 0, 1, 0x41, 7, 7]);
        assert_eq!(held, first);
    }
}
