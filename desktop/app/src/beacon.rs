// SPDX-License-Identifier: GPL-3.0-or-later
//! The BLE proximity beacon frame, shared by every platform (`docs/protocol/discovery.md`).
//!
//! A PC broadcasts a 13-byte manufacturer-data frame under company ID `0xFFFF`:
//! the magic `"NCTR\x01"` and the first 8 bytes of its `DeviceId`. Phones set to
//! "Wake when PC is nearby" scan for it. This module holds only the pure parts,
//! so Windows, Linux and the macOS stub agree on the bytes and the tests need no
//! radio. The code that sends and receives the frame stays in each platform.

use std::collections::HashMap;

use nectarlink_core::DeviceId;

/// Bluetooth SIG test/internal company identifier used for NectarLink proximity frames.
pub const COMPANY_ID: u16 = 0xFFFF;

/// 5-byte magic header (`"NCTR"` + protocol version `0x01`).
pub const MAGIC: &[u8; 5] = b"NCTR\x01";

/// Length in bytes of the full frame: [`MAGIC`] plus an 8-byte device ID prefix.
pub const FRAME_LEN: usize = 13;

/// Near-range RSSI threshold (dBm) that arms walk-away detection.
pub const NEAR_RSSI_DBM: i16 = -72;

/// Walk-away RSSI threshold (dBm) below which `phone_walked_away` fires.
pub const WALK_AWAY_RSSI_DBM: i16 = -85;

/// Rolling BLE RSSI hysteresis tracker per 8-byte peer ID prefix.
#[derive(Debug, Default)]
pub struct RssiTracker {
    peers: HashMap<[u8; 8], PeerRssiState>,
}

#[derive(Debug, Clone, Default)]
struct PeerRssiState {
    samples: [i16; 4],
    count: usize,
    idx: usize,
    armed_near: bool,
}

impl RssiTracker {
    /// Records a new RSSI reading (in dBm) for `prefix` and returns `true` when
    /// the peer transitions from armed-near (`>= -72 dBm`) to walked-away (`<= -85 dBm`).
    pub fn record(&mut self, prefix: [u8; 8], rssi: i16) -> bool {
        let state = self.peers.entry(prefix).or_default();
        state.samples[state.idx] = rssi;
        state.idx = (state.idx + 1) % state.samples.len();
        state.count = (state.count + 1).min(state.samples.len());
        if state.count < state.samples.len() {
            return false;
        }
        let sum: i32 = state.samples.iter().map(|&v| i32::from(v)).sum();
        let avg = (sum / (state.samples.len() as i32)) as i16;
        if avg >= NEAR_RSSI_DBM {
            state.armed_near = true;
            false
        } else if state.armed_near && avg <= WALK_AWAY_RSSI_DBM {
            state.armed_near = false;
            true
        } else {
            false
        }
    }
}

/// Builds the 13-byte manufacturer-specific payload for `device_id`:
/// - Bytes `0..5`: [`MAGIC`] (`b"NCTR\x01"`)
/// - Bytes `5..13`: first 8 bytes of `device_id.as_bytes()` (Ed25519 public key prefix)
pub fn build_manufacturer_payload(device_id: &DeviceId) -> [u8; FRAME_LEN] {
    let mut out = [0u8; FRAME_LEN];
    out[0..5].copy_from_slice(MAGIC);
    out[5..13].copy_from_slice(&device_id.as_bytes()[0..8]);
    out
}

/// Validates the `NCTR\x01` magic header and extracts the peer device ID prefix.
/// Accepts either the 13-byte payload (5-byte magic + 8-byte prefix) or a 12-byte
/// payload (5-byte magic + 7-byte prefix, zero-padded in the 8th byte).
pub fn parse_manufacturer_payload(bytes: &[u8]) -> Option<[u8; 8]> {
    if bytes.len() < 12 || &bytes[0..5] != MAGIC {
        return None;
    }
    let mut prefix = [0u8; 8];
    if bytes.len() >= FRAME_LEN {
        prefix.copy_from_slice(&bytes[5..13]);
    } else {
        prefix[0..7].copy_from_slice(&bytes[5..12]);
    }
    Some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_with(seed: u8) -> ([u8; 32], DeviceId) {
        let mut raw = [0u8; 32];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(seed);
        }
        (raw, DeviceId(raw))
    }

    #[test]
    fn builds_13_byte_manufacturer_payload_with_magic_and_id_prefix() {
        let (raw, id) = id_with(3);
        let payload = build_manufacturer_payload(&id);
        assert_eq!(payload.len(), 13);
        assert_eq!(&payload[0..5], b"NCTR\x01");
        assert_eq!(&payload[5..13], &raw[0..8]);
        assert_eq!(COMPANY_ID, 0xFFFF);
    }

    #[test]
    fn parses_a_13_byte_payload() {
        let (raw, id) = id_with(9);
        let payload = build_manufacturer_payload(&id);
        assert_eq!(parse_manufacturer_payload(&payload), Some(raw[0..8].try_into().unwrap()));
    }

    #[test]
    fn parses_a_12_byte_payload_zero_padded() {
        let (raw, id) = id_with(9);
        let payload = build_manufacturer_payload(&id);
        let parsed = parse_manufacturer_payload(&payload[0..12]).expect("12 bytes parse");
        assert_eq!(&parsed[0..7], &raw[0..7]);
        assert_eq!(parsed[7], 0);
    }

    #[test]
    fn rejects_wrong_magic_and_short_payloads() {
        assert_eq!(parse_manufacturer_payload(b""), None);
        assert_eq!(parse_manufacturer_payload(&[0u8; 11]), None);
        assert_eq!(parse_manufacturer_payload(b"NCTR\x01short"), None);
        assert_eq!(parse_manufacturer_payload(b"NCTR\x0212345678"), None);
        assert_eq!(parse_manufacturer_payload(b"XXXX\x0112345678"), None);
    }

    #[test]
    fn payload_round_trips_through_the_parser() {
        let (raw, id) = id_with(0x10);
        let parsed = parse_manufacturer_payload(&build_manufacturer_payload(&id)).expect("valid payload");
        assert_eq!(&parsed[..], &raw[0..8]);
    }

    #[test]
    fn rssi_tracker_fires_walk_away_only_after_armed_near() {
        let mut tracker = RssiTracker::default();
        let peer = [1, 2, 3, 4, 5, 6, 7, 8];
        // Far readings before ever being near should not trigger walk-away.
        for _ in 0..4 {
            assert!(!tracker.record(peer, -90));
        }
        // Coming into near range arms the tracker.
        for _ in 0..4 {
            assert!(!tracker.record(peer, -65));
        }
        // Walking away below -85 dBm triggers once, then disarms until near again.
        assert!(!tracker.record(peer, -88));
        assert!(!tracker.record(peer, -90));
        assert!(!tracker.record(peer, -92));
        assert!(tracker.record(peer, -90));
        assert!(!tracker.record(peer, -92));
    }

    #[test]
    fn rssi_tracker_arms_at_near_threshold_and_fires_once_on_walk_away() {
        let mut tracker = RssiTracker::default();
        let peer = [9, 8, 7, 6, 5, 4, 3, 2];
        // Exactly at NEAR_RSSI_DBM (-72) for 4 samples arms the tracker.
        for _ in 0..4 {
            assert!(!tracker.record(peer, NEAR_RSSI_DBM));
        }
        // Dropping to WALK_AWAY_RSSI_DBM (-85) across 4 samples triggers once.
        assert!(!tracker.record(peer, WALK_AWAY_RSSI_DBM));
        assert!(!tracker.record(peer, WALK_AWAY_RSSI_DBM));
        assert!(!tracker.record(peer, WALK_AWAY_RSSI_DBM));
        assert!(tracker.record(peer, WALK_AWAY_RSSI_DBM));
        // Subsequent distant samples do not re-trigger without re-arming near.
        for _ in 0..4 {
            assert!(!tracker.record(peer, -95));
        }
    }
}
