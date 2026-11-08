// SPDX-License-Identifier: GPL-3.0-or-later
//! What the Linux tray and the macOS menu-bar item share: both are a Qt
//! `QSystemTrayIcon` with a `QMenu` (`cpp/app_helpers.cpp`, `qt_tray_*`). Qt
//! reports a chosen item as a short text. This turns the text into a
//! [`TrayEvent`] and runs the thread that waits for them.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use crate::win::tray::{MenuLabels, TrayEvent};

/// How long one wait for a menu choice lasts, in milliseconds. After it the
/// thread looks at its stop flag.
const WAIT_MS: u32 = 200;

/// The event for the text `qt_tray_wait_action` returns, or `None` for a text
/// that is no menu item.
pub(crate) fn parse_tray_helper_action(line: &str) -> Option<TrayEvent> {
    match line.trim() {
        "OPEN" => Some(TrayEvent::Open),
        "COMMAND_PALETTE" => Some(TrayEvent::CommandPalette),
        "SHELF" => Some(TrayEvent::Shelf),
        "TAKE_PHOTO" => Some(TrayEvent::TakePhoto),
        "SCAN_DOCUMENT" => Some(TrayEvent::ScanDocument),
        "FIND_PHONE" => Some(TrayEvent::FindPhone),
        "OPEN_LINK" => Some(TrayEvent::OpenLinkOnPhone),
        "PAUSE_NOTIFICATIONS" => Some(TrayEvent::ToggleNotificationsPause),
        "QUIT" => Some(TrayEvent::Quit),
        "APPEARANCE_CHANGED" => Some(TrayEvent::AppearanceChanged),
        _ => None,
    }
}

/// The text of the pause item: it says what a choice does next.
pub(crate) fn pause_label(labels: &MenuLabels, paused: bool) -> &str {
    if paused { &labels.resume_notifications } else { &labels.pause_notifications }
}

/// Calls `on_event` for each menu choice `wait` returns, until `stop` is set.
/// `wait` blocks for a short time and returns the text of a choice, or an
/// empty text when there was none.
fn run_action_loop(stop: &AtomicBool, mut wait: impl FnMut() -> String, on_event: impl Fn(TrayEvent)) {
    while !stop.load(Ordering::Acquire) {
        let action = wait();
        if stop.load(Ordering::Acquire) {
            break;
        }
        if !action.is_empty()
            && let Some(event) = parse_tray_helper_action(&action)
        {
            on_event(event);
        }
    }
}

/// Starts the thread that waits for the Qt menu's choices. It ends soon after
/// `stop` is set.
pub(crate) fn spawn_action_thread(
    stop: Arc<AtomicBool>,
    on_event: impl Fn(TrayEvent) + Send + 'static,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tray-qt-actions".into())
        .spawn(move || {
            run_action_loop(&stop, || crate::bridge::native::ffi::qt_tray_wait_action(WAIT_MS), on_event);
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn parse_tray_helper_action_maps_every_menu_item() {
        assert_eq!(parse_tray_helper_action("OPEN\n"), Some(TrayEvent::Open));
        assert_eq!(parse_tray_helper_action("COMMAND_PALETTE"), Some(TrayEvent::CommandPalette));
        assert_eq!(parse_tray_helper_action("SHELF"), Some(TrayEvent::Shelf));
        assert_eq!(parse_tray_helper_action("TAKE_PHOTO"), Some(TrayEvent::TakePhoto));
        assert_eq!(parse_tray_helper_action("SCAN_DOCUMENT"), Some(TrayEvent::ScanDocument));
        assert_eq!(parse_tray_helper_action("FIND_PHONE"), Some(TrayEvent::FindPhone));
        assert_eq!(parse_tray_helper_action("OPEN_LINK"), Some(TrayEvent::OpenLinkOnPhone));
        assert_eq!(
            parse_tray_helper_action("PAUSE_NOTIFICATIONS"),
            Some(TrayEvent::ToggleNotificationsPause)
        );
        assert_eq!(parse_tray_helper_action("  QUIT \r\n"), Some(TrayEvent::Quit));
        assert_eq!(parse_tray_helper_action("APPEARANCE_CHANGED"), Some(TrayEvent::AppearanceChanged));
    }

    #[test]
    fn parse_tray_helper_action_ignores_anything_else() {
        for line in ["", "   ", "UNKNOWN", "open", "Quit", "OPEN QUIT", "RESUMED", "appearance_changed"] {
            assert_eq!(parse_tray_helper_action(line), None, "{line:?}");
        }
    }

    #[test]
    fn pause_label_says_what_a_choice_does_next() {
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
        assert_eq!(pause_label(&labels, false), "Pause");
        assert_eq!(pause_label(&labels, true), "Resume");
    }

    #[test]
    fn the_loop_delivers_known_choices_in_order_and_skips_the_rest() {
        let stop = AtomicBool::new(false);
        let mut queue =
            vec!["QUIT", "", "NOPE", "OPEN", ""].into_iter().map(String::from).collect::<Vec<_>>();
        queue.reverse();
        let got = RefCell::new(Vec::new());
        run_action_loop(
            &stop,
            || {
                let next = queue.pop().unwrap_or_default();
                if queue.is_empty() {
                    stop.store(true, Ordering::Release);
                }
                next
            },
            |e| got.borrow_mut().push(e),
        );
        // The unknown text and the empty ones are skipped. The last wait sets
        // the stop flag, so what it returns would be dropped.
        assert_eq!(*got.borrow(), vec![TrayEvent::Quit, TrayEvent::Open]);
    }

    #[test]
    fn the_loop_does_not_wait_when_already_stopped() {
        let stop = AtomicBool::new(true);
        let waits = Cell::new(0);
        run_action_loop(
            &stop,
            || {
                waits.set(waits.get() + 1);
                "OPEN".to_string()
            },
            |_| panic!("no event after stop"),
        );
        assert_eq!(waits.get(), 0);
    }
}
