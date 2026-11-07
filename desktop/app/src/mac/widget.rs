// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS widget: the phone status snapshot and the verbs a widget button sends.
//!
//! There is no widget on a Mac yet. A WidgetKit widget needs an extension, so
//! nothing reads the snapshot, and `--widget-provider` exits, as it does on
//! Windows (#85). The snapshot and the verb helpers are here so that a widget
//! can use them when it exists. Until then the app writes no snapshot at
//! startup: [`write_state_snapshot`] does nothing. The snapshot holds the phone's name, whether
//! it is connected and its battery. It never holds notification text,
//! message content, clipboard text or file names.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

use nectarlink_core::LinkState;
use serde::{Deserialize, Serialize};

use crate::{launch::Request, state::DeviceView, win::single_instance};

pub const ARG: &str = "--widget-provider";

/// The primary phone's live status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetState {
    pub connected: bool,
    pub phone_name: String,
    pub battery_level: Option<u8>,
    pub battery_charging: bool,
}

/// Path to `widget-state.json` inside `data_dir`.
pub fn state_snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("widget-state.json")
}

/// Writes `state` to `<data_dir>/widget-state.json`: into a temporary file
/// that only its owner can read (mode 0600), which then replaces the old one,
/// so a reader never sees half a file. It refuses to write over a link.
// Used when a widget exists.
#[cfg_attr(not(test), allow(dead_code))]
pub fn write_snapshot_file(data_dir: &Path, state: &WidgetState) -> io::Result<()> {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fs::create_dir_all(data_dir)?;
    let path = state_snapshot_path(data_dir);
    let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to overwrite a link named widget-state.json",
        ));
    }

    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = data_dir.join(format!(".widget-state.json.tmp.{}.{seq}", std::process::id()));
    let written = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp_path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&json)?;
        file.sync_all()
    })()
    .and_then(|()| fs::rename(&tmp_path, &path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    written
}

/// Reads `<data_dir>/widget-state.json`, or [`WidgetState::default`] when it
/// is missing or damaged.
// Nothing reads the snapshot until a widget exists.
#[cfg_attr(not(test), allow(dead_code))]
pub fn read_snapshot_file(data_dir: &Path) -> WidgetState {
    fs::read(state_snapshot_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WidgetState>(&bytes).ok())
        .unwrap_or_default()
}

/// What the snapshot says about the primary phone: the first one online, or
/// else the first one paired.
// Used when a widget exists.
#[allow(dead_code)]
fn state_of(devices: &[DeviceView]) -> WidgetState {
    let online = |d: &&DeviceView| matches!(d.link, LinkState::Online { .. });
    match devices.iter().find(online).or_else(|| devices.first()) {
        Some(dev) => WidgetState {
            connected: online(&dev),
            phone_name: dev.info.name.clone(),
            battery_level: dev.battery.as_ref().map(|b| b.level),
            battery_charging: dev.battery.as_ref().is_some_and(|b| b.charging),
        },
        None => WidgetState::default(),
    }
}

/// Does nothing: no widget reads `widget-state.json` on a Mac yet, so the app
/// does not write the phone's name and battery to disk. When a widget exists,
/// this writes [`state_of`] with [`write_snapshot_file`].
pub fn write_state_snapshot(_data_dir: &Path) {}

/// There is no widget to host on a Mac.
pub fn run_provider(_data_dir: &Path) -> ExitCode {
    ExitCode::SUCCESS
}

/// Normalizes a widget verb into the task action that
/// `bridge::app::handle_jump_task` understands.
// Used when a widget exists.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn normalize_action_verb(verb: &str) -> Result<&'static str, String> {
    match verb.trim() {
        "send-clipboard" | "send_clipboard" => Ok("send-clipboard"),
        "find-phone" | "find_phone" => Ok("find-phone"),
        "take-photo" | "take_photo" => Ok("take-photo"),
        "open-timeline" | "open_timeline" => Ok("open-timeline"),
        "send-files" | "send-file" | "send_file" => Ok("send-files"),
        "open" | "" => Ok("open"),
        _ => Err("unknown widget action verb".to_owned()),
    }
}

/// Sends a widget verb to the running NectarLink, or starts NectarLink with
/// `--task <action>` when none is running.
// Used when a widget exists.
#[allow(dead_code)]
pub fn handle_action_verb(verb: &str, data_dir: Option<&Path>) -> Result<(), String> {
    let action = normalize_action_verb(verb)?;

    if let Some(dir) = data_dir {
        let request =
            if action == "open" { Request::Show } else { Request::Task { action: action.to_owned() } };
        if crate::launch::queue(dir, &request).is_ok() {
            single_instance::wake(dir);
            return Ok(());
        }
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    if let Some(dir) = data_dir {
        command.arg("--data-dir").arg(dir);
    }
    if action != "open" {
        command.arg(super::jumplist::ARG).arg(action);
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn snapshot_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let state = WidgetState {
            connected: true,
            phone_name: "Galaxy S24".into(),
            battery_level: Some(62),
            battery_charging: true,
        };
        write_snapshot_file(dir.path(), &state).unwrap();
        assert_eq!(read_snapshot_file(dir.path()), state);
        let mode = fs::metadata(state_snapshot_path(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A second write replaces the first and leaves no temporary file.
        let newer = WidgetState { battery_level: None, ..state };
        write_snapshot_file(dir.path(), &newer).unwrap();
        assert_eq!(read_snapshot_file(dir.path()), newer);
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["widget-state.json"]);
    }

    #[test]
    fn missing_or_damaged_snapshot_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_snapshot_file(dir.path()), WidgetState::default());
        fs::write(state_snapshot_path(dir.path()), b"{ not json").unwrap();
        assert_eq!(read_snapshot_file(dir.path()), WidgetState::default());
    }

    #[test]
    fn snapshot_refuses_to_write_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, state_snapshot_path(dir.path())).unwrap();
        assert!(write_snapshot_file(dir.path(), &WidgetState::default()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn snapshot_carries_only_status_never_content() {
        let state = WidgetState {
            connected: true,
            phone_name: "Pixel".into(),
            battery_level: Some(80),
            battery_charging: false,
        };
        let json: serde_json::Value = serde_json::to_value(&state).unwrap();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        // A new field is a decision about what the file may hold: change this list on purpose.
        assert_eq!(keys, ["battery_charging", "battery_level", "connected", "phone_name"]);
    }

    #[test]
    fn verbs_normalize_to_tasks_and_unknown_verbs_fail() {
        for (verb, action) in [
            ("send-clipboard", "send-clipboard"),
            ("send_clipboard", "send-clipboard"),
            (" find-phone ", "find-phone"),
            ("take_photo", "take-photo"),
            ("open-timeline", "open-timeline"),
            ("send-file", "send-files"),
            ("open", "open"),
            ("", "open"),
        ] {
            assert_eq!(normalize_action_verb(verb), Ok(action), "{verb}");
        }
        // The error never repeats what it was given.
        assert_eq!(normalize_action_verb("do-evil"), Err("unknown widget action verb".to_owned()));
    }

    #[test]
    fn every_jump_task_is_a_widget_verb() {
        for action in super::super::jumplist::TASKS {
            assert_eq!(normalize_action_verb(action), Ok(*action));
        }
    }
}
