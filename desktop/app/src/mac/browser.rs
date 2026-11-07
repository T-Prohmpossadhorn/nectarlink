// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS browser and window inspection. Programs are named by bundle id
//! (lowercase, "com.google.chrome"), where Windows uses file names. This
//! file knows which bundle ids are browsers, how an executable's path
//! leads to its app, and how the window list of CoreGraphics (`cg.rs`)
//! becomes `Window` values and rectangles. The page automation and the
//! minimized state are not here yet: they need the Accessibility API.
//!
//! The window list holds the windows on screen, from front to back. A
//! window that is minimized, or on another Space, is not in it. The
//! rectangles are the numbers `kCGWindowBounds` gives. Its header does not
//! state a unit, so they are used as they come (screen space, origin at the
//! top left of the main display). Conversion to pixels belongs where a
//! capture place is made.
//!
//! No title is ever logged, and none is shown by `Debug`.
//!
//! Matches the public API of `desktop/app/src/linux/browser.rs`.

mod cg;

use std::{
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
};

/// Window handle, the shape `linux/browser.rs` has. On macOS it holds a
/// window number (`kCGWindowNumber`, which is 32 bits wide).
#[allow(clippy::upper_case_acronyms)]
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
// Nothing on macOS reads the edges yet: Play on phone is not built on macOS.
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
#[derive(Clone)]
pub struct Window {
    pub hwnd: HWND,
    /// The process that owns the window.
    // Nothing reads it yet: Play on phone is not built on macOS.
    #[allow(dead_code)]
    pub pid: i32,
    /// Its program's bundle id, lowercase ("com.google.chrome"). Empty when
    /// the program has none (a bare binary) or the system would not say.
    pub program: String,
    /// Empty when the system left the title out.
    pub title: String,
}

// By hand, so a title never reaches a log through `{:?}`.
impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Window")
            .field("hwnd", &self.hwnd)
            .field("pid", &self.pid)
            .field("program", &self.program)
            .field("has_title", &!self.title.is_empty())
            .finish()
    }
}

/// What the window list says of one window, read into plain values. A key
/// the system left out is `None`.
struct Record {
    number: Option<i32>,
    layer: Option<i32>,
    pid: Option<i32>,
    /// x, y, width and height of `kCGWindowBounds`.
    bounds: Option<[f64; 4]>,
    title: Option<String>,
}

// By hand, so a title never reaches a log through `{:?}`.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("number", &self.number)
            .field("layer", &self.layer)
            .field("pid", &self.pid)
            .field("bounds", &self.bounds)
            .field("has_title", &self.title.as_ref().is_some_and(|t| !t.is_empty()))
            .finish()
    }
}

/// The smallest window `rect_from_bounds` accepts, in both directions: the
/// same limit as `Automation::page_rect` has on Linux.
const MIN_SIDE: i32 = 160;

/// The rectangle of a window by its bounds, rounded to whole numbers. A zero,
/// negative or not finite size, and an edge that does not fit in 32 bits,
/// give `None`.
fn bounds_to_rect([x, y, width, height]: [f64; 4]) -> Option<RECT> {
    // Beyond this a window is not real, and `as i32` stays exact.
    const LIMIT: f64 = 1.0e9;
    let sane = |v: f64| v.is_finite() && v.abs() < LIMIT;
    if !(sane(x) && sane(y) && sane(width) && sane(height)) {
        return None;
    }
    let rect = RECT {
        left: x.round() as i32,
        top: y.round() as i32,
        right: (x + width).round() as i32,
        bottom: (y + height).round() as i32,
    };
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

/// The rectangle of a window by its bounds, `None` when it is not a real size
/// (see `bounds_to_rect`) or it is under 160 by 160.
fn rect_from_bounds(bounds: [f64; 4]) -> Option<RECT> {
    bounds_to_rect(bounds).filter(|r| r.right - r.left >= MIN_SIDE && r.bottom - r.top >= MIN_SIDE)
}

/// The window of one record, or `None` for one we skip: not on layer 0 (the
/// menu bar, the Dock, overlays), our own, with no size, or with no number or
/// owner. `program_of` names the program of a pid and is asked only for a
/// window we keep. A missing title is an empty one.
fn parse(record: &Record, own_pid: i32, mut program_of: impl FnMut(i32) -> Option<String>) -> Option<Window> {
    let number = record.number.filter(|n| *n > 0)?;
    if record.layer? != 0 {
        return None;
    }
    let pid = record.pid.filter(|p| *p > 0 && *p != own_pid)?;
    bounds_to_rect(record.bounds?)?;
    Some(Window {
        hwnd: window_handle(number),
        pid,
        program: program_of(pid).unwrap_or_default(),
        title: record.title.clone().unwrap_or_default(),
    })
}

fn window_handle(number: i32) -> HWND {
    // The number is read as `i32` and used as `u32`.
    HWND(number as u32 as usize as *mut core::ffi::c_void)
}

fn window_number(hwnd: HWND) -> u32 {
    hwnd.0 as usize as u32
}

/// The bundle id, lowercase, of the app that runs process `pid`. A helper
/// gives its outer app. A bare binary, or a process the system will not
/// describe, gives `None`.
fn program_of(pid: i32) -> Option<String> {
    let app = outer_app(&cg::exe_path(pid)?)?;
    cg::bundle_id(&app).map(|id| id.to_ascii_lowercase()).filter(|id| !id.is_empty())
}

/// The windows on screen, front to back. Empty when there is no GUI session.
pub fn windows() -> Vec<Window> {
    let own_pid = std::process::id() as i32;
    // One lookup for each process, however many windows it has.
    let mut programs: HashMap<i32, Option<String>> = HashMap::new();
    cg::window_records(cg::ON_SCREEN_ONLY | cg::EXCLUDE_DESKTOP_ELEMENTS, 0)
        .iter()
        .filter_map(|record| {
            parse(record, own_pid, |pid| programs.entry(pid).or_insert_with(|| program_of(pid)).clone())
        })
        .collect()
}

/// The window that plays `title` in one of the programs of `player`.
// Nothing calls it yet: Play on phone is not built on macOS.
#[allow(dead_code)]
pub fn window_for(player: &str, title: &str) -> Option<Window> {
    let programs = programs_for(player);
    let all = windows();
    let index = crate::window_match::best_window(
        all.iter().map(|w| (w.program.as_str(), w.title.as_str())),
        &programs,
        title,
    )?;
    all.into_iter().nth(index)
}

/// The rectangle of a window, `None` when the window is gone, has no size, or
/// is under 160 by 160.
// Nothing calls it yet: Play on phone is not built on macOS.
#[allow(dead_code)]
pub fn frame_of(hwnd: HWND) -> Option<RECT> {
    let number = window_number(hwnd);
    if number == 0 {
        return None;
    }
    let records = cg::window_records(cg::INCLUDING_WINDOW, number);
    let record = records.iter().find(|r| r.number.and_then(|n| u32::try_from(n).ok()) == Some(number))?;
    rect_from_bounds(record.bounds?)
}

/// A no-op: the bounds are used as `kCGWindowBounds` gives them. Conversion
/// to pixels is done where a place is made.
// Nothing calls it yet: Play on phone is not built on macOS.
#[allow(dead_code)]
pub fn use_physical_pixels() {}

/// Always `false`. A window that is not in the list is off screen, which also
/// holds for a window on another Space, so that is not read as minimized.
/// The minimized state will come from `AXMinimized` with the Accessibility
/// calls of a later PR.
// Nothing calls it yet: Play on phone is not built on macOS.
#[allow(dead_code)]
pub fn is_minimized(_hwnd: HWND) -> bool {
    false
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

    fn record(number: i32, layer: i32, pid: i32, title: Option<&str>) -> Record {
        Record {
            number: Some(number),
            layer: Some(layer),
            pid: Some(pid),
            bounds: Some([10.0, 20.0, 800.0, 600.0]),
            title: title.map(str::to_owned),
        }
    }

    fn named(_pid: i32) -> Option<String> {
        Some("com.google.chrome".to_owned())
    }

    #[test]
    fn a_normal_record_becomes_a_window() {
        let w = parse(&record(42, 0, 500, Some("a page")), 1, named).expect("a window");
        assert_eq!(window_number(w.hwnd), 42);
        assert_eq!((w.pid, w.program.as_str(), w.title.as_str()), (500, "com.google.chrome", "a page"));
    }

    #[test]
    fn a_missing_title_is_an_empty_one() {
        let w = parse(&record(42, 0, 500, None), 1, named).expect("a window");
        assert_eq!(w.title, "");
    }

    #[test]
    fn a_program_that_cannot_be_named_is_an_empty_program() {
        let w = parse(&record(42, 0, 500, Some("t")), 1, |_| None).expect("a window");
        assert_eq!(w.program, "");
    }

    #[test]
    fn records_off_layer_zero_are_skipped() {
        for layer in [-1, 1, 3, 25] {
            assert!(parse(&record(42, layer, 500, Some("t")), 1, named).is_none(), "{layer}");
        }
    }

    #[test]
    fn our_own_windows_are_skipped() {
        assert!(parse(&record(42, 0, 77, Some("t")), 77, named).is_none());
        assert!(parse(&record(42, 0, 78, Some("t")), 77, named).is_some());
    }

    #[test]
    fn a_record_with_no_size_is_skipped() {
        for bounds in [
            [0.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 100.0, 0.0],
            [0.0, 0.0, 0.0, 100.0],
            [0.0, 0.0, -5.0, 100.0],
            [f64::NAN, 0.0, 100.0, 100.0],
            [0.0, 0.0, f64::INFINITY, 100.0],
        ] {
            let mut r = record(42, 0, 500, Some("t"));
            r.bounds = Some(bounds);
            assert!(parse(&r, 1, named).is_none(), "{bounds:?}");
        }
        let mut r = record(42, 0, 500, Some("t"));
        r.bounds = None;
        assert!(parse(&r, 1, named).is_none());
    }

    #[test]
    fn a_small_window_is_still_listed() {
        let mut r = record(42, 0, 500, Some("t"));
        r.bounds = Some([0.0, 0.0, 100.0, 50.0]);
        assert!(parse(&r, 1, named).is_some());
    }

    #[test]
    fn a_record_with_a_missing_or_bad_key_is_skipped() {
        let base = || record(42, 0, 500, Some("t"));
        let mut r = base();
        r.number = None;
        assert!(parse(&r, 1, named).is_none());
        let mut r = base();
        r.number = Some(0);
        assert!(parse(&r, 1, named).is_none());
        let mut r = base();
        r.number = Some(-3);
        assert!(parse(&r, 1, named).is_none());
        let mut r = base();
        r.layer = None;
        assert!(parse(&r, 1, named).is_none());
        let mut r = base();
        r.pid = None;
        assert!(parse(&r, 1, named).is_none());
        let mut r = base();
        r.pid = Some(0);
        assert!(parse(&r, 1, named).is_none());
    }

    #[test]
    fn the_program_is_looked_up_only_for_a_window_we_keep() {
        let mut asked = Vec::new();
        for r in [record(1, 3, 10, None), record(2, 0, 1, None), record(3, 0, 20, None)] {
            parse(&r, 1, |pid| {
                asked.push(pid);
                None
            });
        }
        assert_eq!(asked, [20]);
    }

    #[test]
    fn a_window_number_survives_the_handle() {
        for number in [1, 42, i32::MAX] {
            assert_eq!(i64::from(window_number(window_handle(number))), i64::from(number));
        }
    }

    #[test]
    fn a_title_is_never_in_debug_output() {
        let secret = "private-title-text";
        let r = record(42, 0, 500, Some(secret));
        assert!(!format!("{r:?}").contains(secret));
        let w = parse(&r, 1, named).expect("a window");
        assert!(!format!("{w:?}").contains(secret));
        assert!(!format!("{:?}", vec![w]).contains(secret));
    }

    #[test]
    fn bounds_give_a_rectangle_by_its_edges() {
        let r = rect_from_bounds([100.0, 200.0, 1280.0, 720.0]).expect("a rectangle");
        assert_eq!((r.left, r.top, r.right, r.bottom), (100, 200, 1380, 920));
        // A window to the left of or above the main display has negative edges.
        let r = rect_from_bounds([-1000.0, -50.0, 800.0, 600.0]).expect("a rectangle");
        assert_eq!((r.left, r.top, r.right, r.bottom), (-1000, -50, -200, 550));
        // Half points round to the nearest edge.
        let r = rect_from_bounds([0.4, 0.6, 200.0, 200.0]).expect("a rectangle");
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 1, 200, 201));
    }

    #[test]
    fn bounds_with_no_size_give_none() {
        for bounds in [
            [0.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 800.0, 0.0],
            [0.0, 0.0, -800.0, 600.0],
            [0.0, 0.0, 800.0, -1.0],
            [f64::NAN, 0.0, 800.0, 600.0],
            [0.0, f64::NEG_INFINITY, 800.0, 600.0],
            [0.0, 0.0, f64::NAN, 600.0],
            [1.0e12, 0.0, 800.0, 600.0],
        ] {
            assert_eq!(rect_from_bounds(bounds), None, "{bounds:?}");
        }
    }

    #[test]
    fn bounds_under_160_by_160_give_none() {
        assert_eq!(rect_from_bounds([0.0, 0.0, 159.0, 600.0]), None);
        assert_eq!(rect_from_bounds([0.0, 0.0, 600.0, 159.0]), None);
        assert_eq!(rect_from_bounds([0.0, 0.0, 100.0, 100.0]), None);
        assert!(rect_from_bounds([0.0, 0.0, 160.0, 160.0]).is_some());
    }
}
