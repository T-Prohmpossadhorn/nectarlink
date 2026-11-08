// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS media session integration stubs. No public API shows what other apps
//! play, so there are no local players here.

use nectarlink_core::{MediaAction, MediaError, MediaPlayer};

pub fn start(_on_change: impl Fn(Vec<MediaPlayer>) + Send + Sync + 'static) {}

/// There is no player to command, so say so rather than claim success.
pub fn command(_player: &str, _action: MediaAction, _pos: Option<u64>) -> Result<(), MediaError> {
    Err(MediaError::Unsupported)
}

pub fn pause_for_call() {}

pub fn resume_after_call() {}

/// Whether the player with bundle id `id` is a video player or a browser.
#[allow(dead_code)]
pub fn says_video(id: &str) -> bool {
    const NEEDLES: [&str; 14] = [
        "com.google.chrome",
        "org.chromium",
        "org.mozilla.firefox",
        "com.apple.safari",
        "com.brave.browser",
        "com.microsoft.edgemac",
        "com.operasoftware.opera",
        "com.vivaldi.vivaldi",
        "org.videolan.vlc",
        "io.mpv",
        "com.colliderli.iina",
        "com.apple.quicktimeplayerx",
        "com.apple.tv",
        "tv.plex",
    ];
    let lower = id.trim().to_ascii_lowercase();
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_video_knows_browsers_and_video_players() {
        assert!(says_video("com.google.Chrome"));
        assert!(says_video("org.videolan.vlc"));
        assert!(!says_video("com.apple.Music"));
        assert!(!says_video("com.spotify.client"));
        assert!(!says_video(""));
    }

    #[test]
    fn command_is_unsupported() {
        assert!(matches!(command("x", MediaAction::Play, None), Err(MediaError::Unsupported)));
    }
}
