// SPDX-License-Identifier: GPL-3.0-or-later
//! The macOS menu-bar item and its menu, through Qt. The hotkey and window
//! functions below are still stubs.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use crate::{bridge::native::ffi, mark};

/// What the user or the system asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    CommandPalette,
    Shelf,
    TakePhoto,
    ScanDocument,
    FindPhone,
    /// Open the link on the clipboard on the phone.
    OpenLinkOnPhone,
    ToggleNotificationsPause,
    Quit,
    /// The Mac woke from sleep. Nothing sends this yet.
    #[allow(dead_code)]
    Resumed,
    /// The look of the system changed. Nothing sends this yet.
    #[allow(dead_code)]
    AppearanceChanged,
}

/// Menu texts (translated by the caller).
#[derive(Debug, Clone)]
pub struct MenuLabels {
    pub open: String,
    pub command_palette: String,
    pub shelf: String,
    pub take_photo: String,
    pub scan_document: String,
    pub find_phone: String,
    pub open_link: String,
    pub pause_notifications: String,
    pub resume_notifications: String,
    pub quit: String,
}

/// Side of the menu-bar picture in pixels. The bar is 18 points high: this is
/// that at twice the resolution.
const ICON_PX: usize = 36;

/// The menu-bar item and its menu: a Qt `QSystemTrayIcon` (see
/// `cpp/app_helpers.cpp`, `qt_tray_*`). A click opens the menu, whose first
/// item is Open.
pub struct Tray {
    labels: MenuLabels,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Tray {
    pub fn create(
        tooltip: &str,
        labels: MenuLabels,
        handler: impl Fn(TrayEvent) + Send + Sync + 'static,
    ) -> Result<Tray, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut thread = None;
        if !cfg!(test) {
            ffi::qt_tray_set_mask_icon(ICON_PX as i32, &mark::render_glyph(ICON_PX));
            ffi::qt_tray_setup(
                "",
                tooltip,
                &labels.open,
                &labels.command_palette,
                &labels.shelf,
                &labels.take_photo,
                &labels.scan_document,
                &labels.find_phone,
                &labels.open_link,
                &labels.pause_notifications,
                &labels.quit,
            );
            thread = crate::qt_tray::spawn_action_thread(Arc::clone(&stop), handler);
            if thread.is_none() {
                // A menu nobody listens to could not even quit the app.
                ffi::qt_tray_teardown();
                return Err("could not start the menu-bar thread".into());
            }
        }
        Ok(Tray { labels, stop, thread })
    }

    pub fn set_tooltip(&self, text: &str) {
        if !cfg!(test) {
            ffi::qt_tray_set_tooltip(text);
        }
    }

    /// Says Pause or Resume in the menu, whichever comes next.
    pub fn set_notifications_paused(&self, paused: bool) {
        if !cfg!(test) {
            ffi::qt_tray_set_pause_label(crate::qt_tray::pause_label(&self.labels, paused));
        }
    }

    /// Whether the items that need a paired phone are on.
    pub fn set_find_phone_enabled(&self, enabled: bool) {
        if !cfg!(test) {
            ffi::qt_tray_set_find_phone_enabled(enabled);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if !cfg!(test) {
            ffi::qt_tray_teardown();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn update_hotkeys(_photo_spec: &str, _scan_spec: &str, _palette_spec: &str, _shelf_spec: &str) {}

#[allow(dead_code)]
pub fn record_foreground_window() {}

pub fn apply_no_activate_style_by_title(_title: &str) {}

pub fn focus_window(_hwnd: isize) -> bool {
    true
}

pub fn is_window_alive_and_visible(_hwnd: isize) -> bool {
    false
}

pub fn snapshot_target_window() -> Option<(isize, String)> {
    None
}

pub fn send_paste_fallback_if_not_foreground(_hwnd: isize) {}

#[allow(dead_code)]
pub fn parse_hotkey(_spec: &str) -> Option<(u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_starts_and_stops_without_qt_in_tests() {
        let labels = MenuLabels {
            open: "Open".into(),
            command_palette: "Palette".into(),
            shelf: "Shelf".into(),
            take_photo: "Photo".into(),
            scan_document: "Scan".into(),
            find_phone: "Find".into(),
            open_link: "Link".into(),
            pause_notifications: "Pause".into(),
            resume_notifications: "Resume".into(),
            quit: "Quit".into(),
        };
        let tray = Tray::create("NectarLink", labels, |_| {}).expect("create tray");
        tray.set_tooltip("NectarLink · 2 devices connected");
        tray.set_notifications_paused(true);
        tray.set_find_phone_enabled(true);
        drop(tray);
    }
}
