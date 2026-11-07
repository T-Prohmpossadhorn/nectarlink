// SPDX-License-Identifier: GPL-3.0-or-later
//! Network change notifications (Wi-Fi switched, cable plugged, VPN up), so
//! paired devices reconnect right away instead of after their backoff. Built
//! on a SystemConfiguration dynamic store ([`super::sc`]); the store calls us
//! on its own dispatch queue, so `watch` can run on any thread.

use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use super::sc::Subscription;

/// Events closer together than this collapse into one trailing call.
const WINDOW: Duration = Duration::from_millis(500);

/// What to do with one incoming event.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Call the callback now.
    Fire,
    /// Call `Debounce::trailing` after this delay.
    Wait(Duration),
    /// Nothing: already waiting, or stopped.
    Drop,
}

/// Debounce state, with the clock passed in so tests need no sleeping.
#[derive(Debug, Default)]
struct Debounce {
    last_fire: Option<Instant>,
    trailing_pending: bool,
    stopped: bool,
}

impl Debounce {
    fn event(&mut self, now: Instant) -> Decision {
        if self.stopped {
            return Decision::Drop;
        }
        match self.last_fire {
            Some(last) if now.saturating_duration_since(last) < WINDOW => {
                if self.trailing_pending {
                    Decision::Drop
                } else {
                    self.trailing_pending = true;
                    Decision::Wait(WINDOW.saturating_sub(now.saturating_duration_since(last)))
                }
            }
            _ => {
                self.last_fire = Some(now);
                Decision::Fire
            }
        }
    }

    /// Whether the trailing call is due now.
    fn trailing(&mut self, now: Instant) -> bool {
        if self.stopped || !self.trailing_pending {
            return false;
        }
        self.trailing_pending = false;
        self.last_fire = Some(now);
        true
    }

    fn stop(&mut self) {
        self.stopped = true;
    }
}

type Callback = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    callback: Callback,
    debounce: Mutex<Debounce>,
}

static SHARED: OnceLock<&'static Shared> = OnceLock::new();
static SUBSCRIPTION: Mutex<Option<Subscription>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn on_event(shared: &'static Shared) {
    let decision = lock(&shared.debounce).event(Instant::now());
    match decision {
        Decision::Fire => (shared.callback)(),
        Decision::Wait(delay) => {
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                if lock(&shared.debounce).trailing(Instant::now()) {
                    (shared.callback)();
                }
            });
        }
        Decision::Drop => {}
    }
}

/// Calls `callback` (on a system thread) whenever the network changes. The
/// first change calls at once; a burst collapses to one more call after
/// 500 ms. A failure is logged and leaves the app without notifications.
pub fn watch(callback: impl Fn() + Send + Sync + 'static) {
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        callback: Arc::new(callback),
        debounce: Mutex::new(Debounce::default()),
    }));
    if SHARED.set(shared).is_err() {
        return; // already watching
    }
    // The leaked `Shared` lives to the end of the process, so a callout that
    // is still running after `stop` stays valid.
    match Subscription::start(Box::new(move || on_event(shared))) {
        Some(sub) => *lock(&SUBSCRIPTION) = Some(sub),
        None => tracing::warn!("can't watch for network changes"),
    }
}

/// Stops the notifications (before exit). Safe if `watch` never ran or failed.
pub fn stop() {
    if let Some(shared) = SHARED.get() {
        lock(&shared.debounce).stop();
    }
    if let Some(sub) = lock(&SUBSCRIPTION).take() {
        sub.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_event_fires_at_once() {
        let mut d = Debounce::default();
        assert_eq!(d.event(Instant::now()), Decision::Fire);
    }

    #[test]
    fn burst_collapses_to_one_trailing_fire() {
        let mut d = Debounce::default();
        let t0 = Instant::now();
        assert_eq!(d.event(t0), Decision::Fire);
        let first = d.event(t0 + Duration::from_millis(100));
        assert_eq!(first, Decision::Wait(Duration::from_millis(400)));
        assert_eq!(d.event(t0 + Duration::from_millis(200)), Decision::Drop);
        assert_eq!(d.event(t0 + Duration::from_millis(499)), Decision::Drop);
        assert!(d.trailing(t0 + Duration::from_millis(500)));
        assert!(!d.trailing(t0 + Duration::from_millis(501)));
    }

    #[test]
    fn event_after_the_window_fires_again() {
        let mut d = Debounce::default();
        let t0 = Instant::now();
        assert_eq!(d.event(t0), Decision::Fire);
        assert_eq!(d.event(t0 + Duration::from_millis(500)), Decision::Fire);
    }

    #[test]
    fn trailing_fire_opens_a_new_window() {
        let mut d = Debounce::default();
        let t0 = Instant::now();
        d.event(t0);
        d.event(t0 + Duration::from_millis(10));
        assert!(d.trailing(t0 + Duration::from_millis(500)));
        assert!(matches!(d.event(t0 + Duration::from_millis(600)), Decision::Wait(_)));
    }

    #[test]
    fn stop_blocks_further_calls() {
        let mut d = Debounce::default();
        let t0 = Instant::now();
        d.event(t0);
        d.event(t0 + Duration::from_millis(10));
        d.stop();
        assert!(!d.trailing(t0 + Duration::from_millis(500)));
        assert_eq!(d.event(t0 + Duration::from_secs(5)), Decision::Drop);
    }

    #[test]
    fn stop_without_watch_is_safe() {
        stop();
    }
}
