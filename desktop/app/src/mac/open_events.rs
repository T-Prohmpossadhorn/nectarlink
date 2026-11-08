// SPDX-License-Identifier: GPL-3.0-or-later
//! Finder open events: files opened with NectarLink and files dropped on its
//! Dock icon. A `nectarlink://` link also arrives here, but nothing handles
//! that scheme yet, so it is ignored. The system sends these to the running app
//! as Apple Events, never in `argv` (so `main.rs` positional paths only matter
//! for a launch from a command line). `cpp/app_helpers.cpp` has a Qt event
//! filter that keeps the URLs of each event; this thread takes them and hands
//! them to the same code as a share from another app (`send_to::handle`).
//!
//! Nothing here logs a path or a link, only counts.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use crate::launch::Request;

/// How long one wait for an event lasts. After it the thread looks at its stop flag.
const WAIT_MS: u32 = 500;
/// A drop of many files is many events. Wait this long after the first so they
/// make one share, not one share each.
const GATHER: Duration = Duration::from_millis(200);

static STOP: AtomicBool = AtomicBool::new(false);

/// The request for the URLs of one gathered open event, or `None` when none of
/// them is a share. Pure: it does not start a send.
pub fn request_from_open_event(urls: &[String]) -> Option<Request> {
    let shared = super::share_target::payload_from_open_event(urls)?;
    Some(Request::Share { files: shared.files, url: shared.url, text: shared.text })
}

/// Handles open events from now on. Quit stays available. This code opens no
/// window itself, but a share may bring the main window forward when it
/// cannot pick one phone.
pub fn start() {
    STOP.store(false, Ordering::Release);
    let spawned = std::thread::Builder::new().name("open-events".into()).spawn(|| {
        while !STOP.load(Ordering::Acquire) {
            let mut urls = crate::bridge::native::ffi::mac_open_urls_wait(WAIT_MS);
            if urls.is_empty() {
                continue;
            }
            std::thread::sleep(GATHER);
            urls.extend(crate::bridge::native::ffi::mac_open_urls_wait(0));
            if STOP.load(Ordering::Acquire) {
                break;
            }
            match request_from_open_event(&urls) {
                Some(request) => {
                    tracing::info!(count = urls.len(), "open event");
                    crate::send_to::handle(request);
                }
                None => tracing::info!(count = urls.len(), "open event with nothing to share"),
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "can't handle Finder open events");
    }
}

/// Stops the thread (at quit). It ends within one wait.
pub fn stop() {
    STOP.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn urls(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn files_make_one_share_request() {
        assert_eq!(
            request_from_open_event(&urls(&["file:///tmp/a%20b.txt", "file:///tmp/c.txt"])),
            Some(Request::Share {
                files: vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")],
                url: None,
                text: None,
            })
        );
    }

    #[test]
    fn a_nectarlink_link_is_not_a_request() {
        assert_eq!(request_from_open_event(&urls(&["nectarlink://share?text=hello"])), None);
    }

    #[test]
    fn empty_input_gives_none() {
        assert_eq!(request_from_open_event(&[]), None);
        assert_eq!(request_from_open_event(&urls(&["https://example.com", "plain text"])), None);
    }
}
