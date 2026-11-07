// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS browser and window inspection. Programs are named by bundle id
//! (lowercase, "com.google.chrome"), where Windows uses file names. This
//! file knows which bundle ids are browsers, and how an executable's path
//! leads to its app. The window list, the rectangles and
//! the page automation are stubs until later PRs.
//!
//! Matches the public API of `desktop/app/src/linux/browser.rs`.

use std::path::{Component, Path, PathBuf};

/// Window handle, the shape `linux/browser.rs` has. On macOS it will hold a
/// window number.
// Nothing on macOS makes one yet: the window list (PR 15) does.
#[allow(dead_code, clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HWND(pub *mut core::ffi::c_void);

// SAFETY: a window handle is an integer ID held in a pointer-sized field.
// Nothing ever reads through it.
#[allow(unsafe_code)]
unsafe impl Send for HWND {}
// SAFETY: as for `Send`.
#[allow(unsafe_code)]
unsafe impl Sync for HWND {}

/// Rectangle by its edges, the shape `linux/browser.rs` has.
// Nothing on macOS makes one yet: the window list (PR 15) does.
#[allow(dead_code, clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Bundle ids of the browsers, lowercase. A match is exact: a Chrome
/// variant (Canary, Beta) or a Firefox edition has its own id and is not
/// listed.
const BROWSERS: [&str; 8] = [
    "com.apple.safari",
    "com.google.chrome",
    "com.microsoft.edgemac",
    "org.mozilla.firefox",
    "com.brave.browser",
    "company.thebrowser.browser",
    "com.vivaldi.vivaldi",
    "com.operasoftware.opera",
];

/// The programs that may own the window of `player`, a media session's
/// player id. On macOS that is the bundle id itself, lowercase. A blank id
/// names no program.
pub fn programs_for(player: &str) -> Vec<String> {
    let id = player.trim().to_ascii_lowercase();
    if id.is_empty() { Vec::new() } else { vec![id] }
}

/// Whether `program`, a lowercase or mixed-case bundle id, is a known browser.
/// Only the exact ids of `BROWSERS` count: a player is not a browser, and a
/// made-up id that contains a browser's name is not one either.
pub fn is_browser(program: &str) -> bool {
    let id = program.trim().to_ascii_lowercase();
    BROWSERS.contains(&id.as_str())
}

/// The outermost `.app` folder on the path of `exe`, only when the path is
/// absolute and a `Contents` folder follows that `.app`. A helper inside an
/// app (`Chrome.app/.../Chrome Helper.app/Contents/MacOS/x`) gives the app
/// that holds it. This is the conservative choice: a media session is
/// expected to name the app, but which id a real helper process reports is
/// not yet known (a hardware check). A bare binary or a process in a
/// framework or an XPC service with no app around it gives `None`.
// Nothing calls it yet: the window list (PR 15) finds a window's program
// with it.
#[allow(dead_code)]
pub fn outer_app(exe: &Path) -> Option<PathBuf> {
    if !exe.is_absolute() {
        return None;
    }
    let parts: Vec<Component<'_>> = exe.components().collect();
    let is_app = |c: &Component<'_>| match c {
        Component::Normal(name) => Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("app")),
        _ => false,
    };
    let index = parts.iter().position(is_app)?;
    let has_contents = matches!(parts.get(index + 1), Some(Component::Normal(next)) if *next == "Contents");
    has_contents.then(|| parts[..=index].iter().collect())
}

/// A visible window.
#[derive(Debug, Clone)]
pub struct Window {
    // Nothing reads these yet: the window list (PR 15) fills them in.
    #[allow(dead_code)]
    pub hwnd: HWND,
    /// The process that owns the window.
    #[allow(dead_code)]
    pub pid: i32,
    /// Its program's bundle id, lowercase ("com.google.chrome").
    pub program: String,
    pub title: String,
}

pub fn windows() -> Vec<Window> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_id_is_its_lowercase_bundle_id() {
        assert_eq!(programs_for("com.google.Chrome"), ["com.google.chrome"]);
        assert_eq!(programs_for("  org.videolan.VLC "), ["org.videolan.vlc"]);
        assert!(programs_for("").is_empty());
        assert!(programs_for("   ").is_empty());
    }

    #[test]
    fn every_known_browser_is_one_in_any_case() {
        for id in [
            "com.apple.Safari",
            "com.google.Chrome",
            "com.microsoft.edgemac",
            "org.mozilla.firefox",
            "com.brave.Browser",
            "company.thebrowser.Browser",
            "com.vivaldi.Vivaldi",
            "com.operasoftware.Opera",
            " com.apple.safari ",
        ] {
            assert!(is_browser(id), "{id}");
        }
    }

    #[test]
    fn players_are_not_browsers() {
        for id in ["org.videolan.vlc", "io.mpv", "com.colliderli.iina"] {
            assert!(!is_browser(id), "{id}");
        }
    }

    #[test]
    fn a_made_up_id_with_a_browser_name_in_it_is_not_a_browser() {
        for id in [
            "com.example.knowledgebase",
            "com.example.edge",
            "com.example.safari",
            "com.apple.safari.helper",
            "com.google.chrome.canary",
            "com.example.firefoxy",
            "chrome",
            "firefox.exe",
            "",
        ] {
            assert!(!is_browser(id), "{id}");
        }
    }

    #[test]
    fn a_helper_inside_an_app_gives_the_outer_app() {
        assert_eq!(
            outer_app(Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")),
            Some(PathBuf::from("/Applications/Google Chrome.app"))
        );
        // A nested helper walks out to the app that holds it.
        let helper = "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/\
                      Versions/1/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)";
        assert_eq!(outer_app(Path::new(helper)), Some(PathBuf::from("/Applications/Google Chrome.app")));
        // The extension is not case sensitive, and a folder may hold a space.
        assert_eq!(
            outer_app(Path::new("/Users/me/My Apps/Some.APP/Contents/MacOS/x")),
            Some(PathBuf::from("/Users/me/My Apps/Some.APP"))
        );
    }

    #[test]
    fn a_path_with_no_app_around_it_gives_none() {
        for bad in [
            // A bare binary, such as a Homebrew mpv.
            "/opt/homebrew/bin/mpv",
            // An assumed example of a page process of Safari, run from a
            // framework's XPC service. The real path was not measured.
            "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/\
             com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent",
            // A folder that only has "app" in its name, or no Contents after the app.
            "/Users/me/app/bin/x",
            "/Applications/Some.app/x",
            "/Applications/Some.app",
            // Not an absolute path.
            "Some.app/Contents/MacOS/x",
            "",
        ] {
            assert_eq!(outer_app(Path::new(bad)), None, "{bad}");
        }
    }
}
