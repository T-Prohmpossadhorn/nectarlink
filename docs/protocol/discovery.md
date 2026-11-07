# Proximity beacon (BLE)

> Status: **draft.** Not part of the QUIC protocol. Breaking changes are
> allowed until v1. License: CC BY 4.0.

A PC can broadcast a short Bluetooth Low Energy frame. A phone that has
"Wake when PC is nearby" turned on scans for it. When the phone hears the
frame, Android wakes NectarLink so it can look for the PC on the network.
The frame only says "a NectarLink PC is near". It carries no secret and it
proves nothing about who sent it. Pairing and the QUIC session do the trust.

## 1. The frame

The PC sends one manufacturer-specific data record. It is a broadcast: it is
not connectable.

| Field | Value |
|---|---|
| Company ID | `0xFFFF` (the Bluetooth SIG test and internal value) |
| Data length | 13 bytes |
| Bytes 0 to 4 | Magic `4E 43 54 52 01`: the text `NCTR` and the version byte `0x01` |
| Bytes 5 to 12 | The first 8 bytes of the PC's 32-byte device ID |

A reader accepts two lengths:

- 13 bytes: the magic and an 8-byte prefix.
- 12 bytes: the magic and a 7-byte prefix. The 8th prefix byte is zero.

A reader ignores any other length, and any frame with a different magic or
version byte.

The device ID is a public key, so the prefix is not secret. Anyone near the PC
can see it and can learn that the PC is there.

## 2. Distance thresholds

A PC that watches for phone frames smooths the signal strength (RSSI) of each
peer over its last 4 readings. It uses two thresholds and a hysteresis:

| Name | Value | Meaning |
|---|---|---|
| Near | `-72 dBm` or stronger | Arms walk-away detection for that peer |
| Walk away | `-85 dBm` or weaker | Fires the `phone_walked_away` automation trigger once, and only after the peer was near |

After it fires, the peer must be near again before it can fire again.

## 3. Who sends and who listens

| Platform | Sends the frame | Watches for frames |
|---|---|---|
| Windows | Yes, from app start (`BluetoothLEAdvertisementPublisher`) | Code exists, but it is off: phones do not send the frame yet |
| Linux | Yes, through BlueZ over D-Bus | Only when an automation rule uses `phone_walked_away` (reads signal strength of paired phones from BlueZ) |
| macOS | **No.** | **No.** |
| Android | No | Yes, as a Companion Device filter on the PC's frame |

macOS does not send the frame. CoreBluetooth lets an app advertise only a
local name and service UUIDs, not manufacturer data. A phone with "Wake when
PC is nearby" on will therefore not find a Mac. The Mac app logs one debug
line when it starts and does nothing else.

The `phone_walked_away` trigger exists in the automation engine, but the app
does not offer it in the interface yet.

## 4. Where the code is

- `desktop/app/src/beacon.rs`: the constants, the frame builder and parser,
  and the RSSI tracker. It has no platform code and its tests need no radio.
- `desktop/app/src/win/ble_beacon.rs`, `desktop/app/src/linux/ble_beacon.rs`
  and `desktop/app/src/mac/ble_beacon.rs`: the platform code.
- `android/app/src/main/kotlin/app/nectarlink/android/companion/PcCompanion.kt`:
  the phone side. It builds the same 13 bytes again, with its own copy of the
  constants. That copy must change together with this page.

## 5. What is not proven

- No real Bluetooth radio test has run for this frame on a phone and a PC
  together. The Android emulator has no Bluetooth LE radio.
- It is not known whether a Mac could be heard by a phone in any other way.
  Nobody has tested that.
