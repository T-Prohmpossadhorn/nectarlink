// SPDX-License-Identifier: GPL-3.0-or-later
//! PC BLE proximity beacon (`docs/protocol/discovery.md`): broadcasts a compact
//! 13-byte Bluetooth Low Energy manufacturer-data frame (`COMPANY_ID = 0xFFFF`,
//! `"NCTR\x01"` + 8-byte `DeviceId` prefix) so nearby phones can wake discovery
//! and reconnect immediately when in range. The frame bytes and the RSSI tracker
//! are shared with the other platforms in `crate::beacon`.

#![allow(dead_code)]

use std::sync::{Mutex, OnceLock};

use nectarlink_core::DeviceId;

use crate::beacon::{COMPANY_ID, RssiTracker, build_manufacturer_payload, parse_manufacturer_payload};
use windows::{
    Devices::Bluetooth::Advertisement::{
        BluetoothLEAdvertisementPublisher, BluetoothLEAdvertisementPublisherStatus,
        BluetoothLEAdvertisementPublisherStatusChangedEventArgs, BluetoothLEAdvertisementReceivedEventArgs,
        BluetoothLEAdvertisementWatcher, BluetoothLEManufacturerData,
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter},
};

static PUBLISHER: OnceLock<Mutex<Option<BluetoothLEAdvertisementPublisher>>> = OnceLock::new();
static WATCHER: OnceLock<Mutex<Option<BluetoothLEAdvertisementWatcher>>> = OnceLock::new();
static RSSI_TRACKER: OnceLock<Mutex<RssiTracker>> = OnceLock::new();

fn publisher_slot() -> &'static Mutex<Option<BluetoothLEAdvertisementPublisher>> {
    PUBLISHER.get_or_init(|| Mutex::new(None))
}

fn watcher_slot() -> &'static Mutex<Option<BluetoothLEAdvertisementWatcher>> {
    WATCHER.get_or_init(|| Mutex::new(None))
}

fn rssi_tracker() -> &'static Mutex<RssiTracker> {
    RSSI_TRACKER.get_or_init(|| Mutex::new(RssiTracker::default()))
}

/// Starts advertising the NectarLink BLE proximity beacon for `device_id`.
///
/// The RSSI watcher (`try_start_watcher`) stays off: phones don't broadcast a
/// beacon yet, so it would scan all the time and only ever hear other PCs.
pub fn start(device_id: DeviceId) {
    // Phones find this PC by it ("Wake when PC is nearby"), so say when it can't run.
    if let Err(e) = try_start(&device_id) {
        tracing::warn!(error = %e, "this PC can't broadcast its Bluetooth beacon");
    }
}

/// Stops the active BLE proximity beacon publisher and RSSI watcher, if running.
pub fn stop() {
    if let Ok(mut guard) = publisher_slot().lock()
        && let Some(publisher) = guard.take()
    {
        let _ = publisher.Stop();
    }
    if let Ok(mut guard) = watcher_slot().lock()
        && let Some(watcher) = guard.take()
    {
        let _ = watcher.Stop();
    }
}

fn try_start(device_id: &DeviceId) -> windows::core::Result<()> {
    let payload = build_manufacturer_payload(device_id);
    let writer = DataWriter::new()?;
    writer.WriteBytes(&payload)?;
    let buffer = writer.DetachBuffer()?;

    let mfg_data = BluetoothLEManufacturerData::Create(COMPANY_ID, &buffer)?;
    let publisher = BluetoothLEAdvertisementPublisher::new()?;
    publisher.Advertisement()?.ManufacturerData()?.Append(&mfg_data)?;
    // Windows can accept Start and stop a moment later (an adapter that can't
    // advertise, Bluetooth turned off), so log why it stops.
    publisher.StatusChanged(&TypedEventHandler::<
        BluetoothLEAdvertisementPublisher,
        BluetoothLEAdvertisementPublisherStatusChangedEventArgs,
    >::new(|_, args| {
        if let Some(args) = args.as_ref()
            && args.Status()? == BluetoothLEAdvertisementPublisherStatus::Aborted
        {
            tracing::warn!(error = ?args.Error()?, "the Bluetooth beacon stopped");
        }
        Ok(())
    }))?;
    publisher.Start()?;

    if let Ok(mut guard) = publisher_slot().lock()
        && let Some(prev) = guard.replace(publisher)
    {
        let _ = prev.Stop();
    }
    tracing::info!("broadcasting the Bluetooth beacon");
    Ok(())
}

fn try_start_watcher(own_id: DeviceId) -> windows::core::Result<()> {
    let own_prefix: [u8; 8] = own_id.as_bytes()[0..8].try_into().unwrap_or([0u8; 8]);
    let watcher = BluetoothLEAdvertisementWatcher::new()?;
    watcher.Received(&TypedEventHandler::<
        BluetoothLEAdvertisementWatcher,
        BluetoothLEAdvertisementReceivedEventArgs,
    >::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            let rssi = args.RawSignalStrengthInDBm().unwrap_or(-127);
            if let Ok(adv) = args.Advertisement()
                && let Ok(list) = adv.GetManufacturerDataByCompanyId(COMPANY_ID)
                && let Ok(count) = list.Size()
            {
                for i in 0..count {
                    if let Ok(item) = list.GetAt(i)
                        && let Ok(buf) = item.Data()
                        && let Ok(len) = buf.Length()
                        && len == 13
                        && let Ok(reader) = DataReader::FromBuffer(&buf)
                    {
                        let mut bytes = [0u8; 13];
                        if reader.ReadBytes(&mut bytes).is_ok()
                            && let Some(prefix) = parse_manufacturer_payload(&bytes)
                            && prefix != own_prefix
                            && let Ok(mut tracker) = rssi_tracker().lock()
                            && tracker.record(prefix, rssi)
                        {
                            crate::automations::fire_trigger(crate::automations::TRIGGER_PHONE_WALKED_AWAY);
                        }
                    }
                }
            }
        }
        Ok(())
    }))?;
    watcher.Start()?;
    if let Ok(mut guard) = watcher_slot().lock()
        && let Some(prev) = guard.replace(watcher)
    {
        let _ = prev.Stop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_stop_never_panic_when_ble_radio_is_absent_or_present() {
        let id = DeviceId([0x42; 32]);
        start(id);
        stop();
    }
}
