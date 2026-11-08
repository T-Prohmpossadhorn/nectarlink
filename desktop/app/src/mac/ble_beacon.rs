// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS BLE beacon: not built yet, so `start` and `stop` only log one line.
//!
//! Windows and Linux broadcast a manufacturer-data frame (`docs/protocol/discovery.md`).
//! CoreBluetooth cannot send one: a Mac peripheral can advertise only a local name
//! and service UUIDs. So a Mac does not broadcast the beacon, and a phone set to
//! "Wake when PC is nearby" will not find it.
//!
//! The two mappers below turn the raw values CoreBluetooth reports into types
//! this app owns. They have no caller yet, because no Bluetooth probe is built.
//! The frame bytes live in `crate::beacon`.

use nectarlink_core::DeviceId;

/// The beacon is not available on macOS. The device ID is not logged.
pub fn start(_device_id: DeviceId) {
    tracing::debug!("the Bluetooth beacon is not available on macOS");
}

pub fn stop() {
    tracing::debug!("the Bluetooth beacon is not running on macOS");
}

/// `CBManagerAuthorization`: whether the user let this app use Bluetooth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// No probe calls this yet, so only the tests build it on a Mac.
#[allow(dead_code)]
pub enum BluetoothAuthorization {
    NotDetermined,
    Restricted,
    Denied,
    AllowedAlways,
    /// A raw value this build does not know, so a newer macOS cannot be misread.
    Unknown,
}

/// Maps a raw `CBManagerAuthorization` value (`NSInteger`).
#[allow(dead_code)]
pub fn authorization_from_raw(raw: i64) -> BluetoothAuthorization {
    match raw {
        0 => BluetoothAuthorization::NotDetermined,
        1 => BluetoothAuthorization::Restricted,
        2 => BluetoothAuthorization::Denied,
        3 => BluetoothAuthorization::AllowedAlways,
        _ => BluetoothAuthorization::Unknown,
    }
}

/// `CBManagerState`: what the Bluetooth radio can do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BluetoothState {
    Unknown,
    Resetting,
    Unsupported,
    Unauthorized,
    PoweredOff,
    PoweredOn,
}

/// Maps a raw `CBManagerState` value (`NSInteger`). A value this build does not
/// know maps to [`BluetoothState::Unknown`].
#[allow(dead_code)]
pub fn state_from_raw(raw: i64) -> BluetoothState {
    match raw {
        1 => BluetoothState::Resetting,
        2 => BluetoothState::Unsupported,
        3 => BluetoothState::Unauthorized,
        4 => BluetoothState::PoweredOff,
        5 => BluetoothState::PoweredOn,
        _ => BluetoothState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_maps_the_four_known_values() {
        assert_eq!(authorization_from_raw(0), BluetoothAuthorization::NotDetermined);
        assert_eq!(authorization_from_raw(1), BluetoothAuthorization::Restricted);
        assert_eq!(authorization_from_raw(2), BluetoothAuthorization::Denied);
        assert_eq!(authorization_from_raw(3), BluetoothAuthorization::AllowedAlways);
    }

    #[test]
    fn authorization_maps_an_unknown_raw_value_to_unknown() {
        assert_eq!(authorization_from_raw(4), BluetoothAuthorization::Unknown);
        assert_eq!(authorization_from_raw(-1), BluetoothAuthorization::Unknown);
        assert_eq!(authorization_from_raw(i64::MAX), BluetoothAuthorization::Unknown);
    }

    #[test]
    fn state_maps_the_six_known_values() {
        assert_eq!(state_from_raw(0), BluetoothState::Unknown);
        assert_eq!(state_from_raw(1), BluetoothState::Resetting);
        assert_eq!(state_from_raw(2), BluetoothState::Unsupported);
        assert_eq!(state_from_raw(3), BluetoothState::Unauthorized);
        assert_eq!(state_from_raw(4), BluetoothState::PoweredOff);
        assert_eq!(state_from_raw(5), BluetoothState::PoweredOn);
    }

    #[test]
    fn state_maps_an_unknown_raw_value_to_unknown() {
        assert_eq!(state_from_raw(6), BluetoothState::Unknown);
        assert_eq!(state_from_raw(-1), BluetoothState::Unknown);
    }

    #[test]
    fn start_and_stop_never_panic() {
        start(DeviceId([0x42; 32]));
        stop();
    }
}
