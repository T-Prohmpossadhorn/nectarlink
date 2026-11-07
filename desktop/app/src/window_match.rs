// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure helpers for finding the window a player shows in: which window of a
//! list best fits a media session, whether a piece of text is a web address,
//! and the rectangle arithmetic Play on phone does with a window's frame.
//!
//! Nothing here asks the system anything, so every platform shares it and
//! its tests run on all of them. The platform modules (`win/browser.rs`,
//! `linux/browser.rs`, and later `mac/browser.rs`) list the windows and
//! read the rectangles; this module only compares and calculates.

/// Which window of `windows` (program, title) most likely
/// plays `title` in one of `programs` (lowercase program names, as the
/// platform's `programs_for` gives them). Returns its index.
///
/// A window's score is 100 and more when its program and its title both
/// fit, 60 and more when its program fits and at least two words of the
/// title appear in its title, 50 and more when only its title fits (and
/// the media's title has 6 or more bytes), and 20 when only its program
/// fits. The highest score wins; the first window of the list wins a tie, so
/// a list from front to back gives the front-most one.
// Linux uses it today; macOS gets a caller with the window list.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn best_window<'a>(
    windows: impl IntoIterator<Item = (&'a str, &'a str)>,
    programs: &[String],
    title: &str,
) -> Option<usize> {
    let needle = title.trim().to_ascii_lowercase();
    let mut best: Option<(i32, usize)> = None;
    for (index, (program, window_title)) in windows.into_iter().enumerate() {
        let prog_match = programs.iter().any(|p| {
            let p_stem = p.trim_end_matches(".exe");
            let w_stem = program.trim_end_matches(".exe");
            !w_stem.is_empty() && (w_stem == p_stem || program == p)
        });
        let wtitle = window_title.to_ascii_lowercase();
        let title_match = !needle.is_empty() && wtitle.contains(&needle);
        let word_hits = if needle.is_empty() {
            0
        } else {
            needle.split_whitespace().filter(|word| word.len() >= 3 && wtitle.contains(*word)).count() as i32
        };

        let score = match (prog_match, title_match) {
            (true, true) => 100 + word_hits,
            (true, false) if word_hits >= 2 => 60 + word_hits,
            (false, true) if needle.len() >= 6 => 50 + word_hits,
            (true, false) => 20,
            _ => 0,
        };
        if score > 0 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, index));
        }
    }
    best.map(|(_, index)| index)
}

/// An address bar's text as a web address: "youtube.com/watch?v=x" becomes
/// "https://youtube.com/watch?v=x". Anything that isn't a web page (a
/// search being typed, a browser's own pages) is `None`.
// Windows uses it today; macOS gets a caller with the address bar reading.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn with_scheme(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Some(text.to_owned());
    }
    if lower.contains("://") || lower.starts_with("about:") {
        return None;
    }
    let host = lower.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    (host.contains('.') || host == "localhost").then(|| format!("https://{text}"))
}

/// Whether `s` looks like a web address, with or without its scheme.
// No platform calls it yet: the macOS window list (PR 15) will. Linux had
// the same function without a caller.
#[allow(dead_code)]
pub fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return false;
    }
    if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        return rest.contains('.') || rest.starts_with("localhost");
    }
    let host = s.split('/').next().unwrap_or("");
    let Some((left, right)) = host.rsplit_once('.') else {
        return false;
    };
    !left.is_empty()
        && right.len() >= 2
        && right.chars().all(|c| c.is_ascii_alphabetic())
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// A rectangle by its edges, as the platforms' `RECT` types have them.
/// `right` and `bottom` are the first column and row outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Windows and Linux use it through Play on phone; macOS gets a caller with
// the window list.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    /// This rectangle in the pixels of `frame`, counted from the frame's
    /// top-left corner. Nothing is cut off: a rectangle that is partly
    /// outside the frame keeps edges below zero, or beyond the frame's size.
    pub fn relative_to(self, frame: Rect) -> Rect {
        Rect {
            left: self.left - frame.left,
            top: self.top - frame.top,
            right: self.right - frame.left,
            bottom: self.bottom - frame.top,
        }
    }

    /// This rectangle with each edge moved from points to pixels at `scale`
    /// pixels per point. Each edge is rounded on its own, so two rectangles
    /// that share an edge in points still share it in pixels.
    // Nothing calls it yet: the macOS window list (PR 15) converts a
    // window's frame (in points) with it.
    #[allow(dead_code)]
    pub fn scaled(self, scale: f64) -> Rect {
        Rect {
            left: to_pixels(self.left, scale),
            top: to_pixels(self.top, scale),
            right: to_pixels(self.right, scale),
            bottom: to_pixels(self.bottom, scale),
        }
    }
}

/// A length or place in points as pixels at `scale` pixels per point,
/// rounded to the nearest pixel (halves away from zero). A scale that is
/// not a positive number counts as 1.0.
// Nothing calls it yet: the macOS window list (PR 15) will.
#[allow(dead_code)]
pub fn to_pixels(points: i32, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (f64::from(points) * scale).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    /// The index of the best window of `list` for `title` in `names`.
    fn pick(list: &[(&str, &str)], names: &[&str], title: &str) -> Option<usize> {
        best_window(list.iter().copied(), &programs(names), title)
    }

    #[test]
    fn program_and_title_match_beat_everything() {
        let list = [
            ("firefox", "A Song - YouTube - Firefox"),
            ("chrome", "A Song - YouTube - Google Chrome"),
            ("chrome", "Mail - Google Chrome"),
        ];
        assert_eq!(pick(&list, &["chrome"], "A Song"), Some(1));
        // The ".exe" of a Windows name does not matter, nor the case of the title.
        let list = [("chrome.exe", "A SONG - YouTube")];
        assert_eq!(pick(&list, &["chrome"], "a song"), Some(0));
        assert_eq!(pick(&list, &["chrome.exe"], "a song"), Some(0));
    }

    #[test]
    fn a_program_alone_scores_lowest_but_counts() {
        let list = [("firefox", "News"), ("chrome", "Mail")];
        assert_eq!(pick(&list, &["chrome"], "A Song"), Some(1));
        assert_eq!(pick(&list, &["chrome"], ""), Some(1));
        // No program, no title: no window.
        assert_eq!(pick(&list, &["vlc"], "A Song"), None);
        // A window without a program name never fits a program.
        assert_eq!(pick(&[("", "Mail")], &["chrome"], ""), None);
    }

    #[test]
    fn two_title_words_in_the_right_program_beat_the_program_alone() {
        // The title is cut ("Great Big Song" is "Great Big Song (Live)" in the tab).
        let list = [("chrome", "Mail"), ("chrome", "Big Song Great - YouTube")];
        assert_eq!(pick(&list, &["chrome"], "Great Big Songs"), Some(1));
        // One word is not enough: the first window of the program stays.
        let list = [("chrome", "Mail"), ("chrome", "Song - YouTube")];
        assert_eq!(pick(&list, &["chrome"], "Great Big Songs"), Some(0));
    }

    #[test]
    fn a_title_alone_needs_six_bytes() {
        // A hashed player ID names no program: only the title finds it.
        let list = [("zen", "A Trailer - YouTube - Zen Browser")];
        assert_eq!(pick(&list, &[], "A Trailer"), Some(0));
        let list = [("zen", "Abc - YouTube - Zen Browser")];
        assert_eq!(pick(&list, &[], "Abc"), None);
        assert_eq!(pick(&list, &[], "Abcde"), None);
        let list = [("zen", "Abcdef - YouTube - Zen Browser")];
        assert_eq!(pick(&list, &[], "Abcdef"), Some(0));
    }

    #[test]
    fn the_best_score_wins_and_the_front_window_wins_a_tie() {
        // Title only (50) in front of program and title (100): the second wins.
        let list = [("zen", "A Song - Zen"), ("chrome", "A Song - Chrome")];
        assert_eq!(pick(&list, &["chrome"], "A Song"), Some(1));
        // The same score twice: the first, that is the front-most one.
        let list = [("chrome", "A Song - One"), ("chrome", "A Song - Two"), ("firefox", "Mail")];
        assert_eq!(pick(&list, &["chrome"], "A Song"), Some(0));
        let list = [("chrome", "Mail"), ("chrome", "News")];
        assert_eq!(pick(&list, &["chrome"], "A Song"), Some(0));
        assert_eq!(pick(&[], &["chrome"], "A Song"), None);
    }

    #[test]
    fn address_bar_text_becomes_a_web_address() {
        assert_eq!(
            with_scheme("youtube.com/watch?v=abc").as_deref(),
            Some("https://youtube.com/watch?v=abc")
        );
        assert_eq!(
            with_scheme("https://www.netflix.com/watch/1").as_deref(),
            Some("https://www.netflix.com/watch/1")
        );
        assert_eq!(with_scheme("127.0.0.1:8765").as_deref(), Some("https://127.0.0.1:8765"));
        assert_eq!(with_scheme("localhost:3000/x").as_deref(), Some("https://localhost:3000/x"));
        assert_eq!(with_scheme("  HTTP://Example.com ").as_deref(), Some("HTTP://Example.com"));
        assert_eq!(with_scheme("cats on skateboards"), None, "a search being typed");
        assert_eq!(with_scheme("chrome://settings"), None);
        assert_eq!(with_scheme("about:blank"), None);
        assert_eq!(with_scheme("settings"), None);
        assert_eq!(with_scheme(""), None);
    }

    #[test]
    fn spots_urls_with_or_without_scheme() {
        assert!(looks_like_url("https://www.youtube.com/watch?v=abc"));
        assert!(looks_like_url("http://localhost:3000"));
        assert!(looks_like_url("youtube.com/watch?v=abc"));
        assert!(looks_like_url("bilibili.com/video/BV1xx"));
        assert!(!looks_like_url("Search Google or type a URL"));
        assert!(!looks_like_url("settings"));
        assert!(!looks_like_url("file.x"), "a one-letter ending is not a domain");
        assert!(looks_like_url("https://localhost"), "after a scheme, a dot or localhost is enough");
        assert!(!looks_like_url("https://intranet"), "after a scheme, a host needs a dot or is localhost");
        assert!(!looks_like_url(""));
    }

    #[test]
    fn a_length_is_scaled_and_rounded() {
        assert_eq!(to_pixels(101, 1.0), 101);
        assert_eq!(to_pixels(101, 2.0), 202);
        assert_eq!(to_pixels(100, 1.5), 150);
        // Exactly half a pixel: away from zero.
        assert_eq!(to_pixels(101, 1.5), 152);
        assert_eq!(to_pixels(-101, 1.5), -152);
        assert_eq!(to_pixels(0, 2.0), 0);
        // Not a usable scale: unchanged.
        assert_eq!(to_pixels(101, 0.0), 101);
        assert_eq!(to_pixels(101, -2.0), 101);
        assert_eq!(to_pixels(101, f64::NAN), 101);
        assert_eq!(to_pixels(101, f64::INFINITY), 101);
    }

    #[test]
    fn a_rectangle_is_scaled_edge_by_edge() {
        let r = Rect::new(10, 20, 110, 70);
        assert_eq!(r.scaled(1.0), r);
        assert_eq!(r.scaled(2.0), Rect::new(20, 40, 220, 140));
        // Edges 3 and 5 of 1.5 land on 4.5 and 7.5: each rounds on its own.
        assert_eq!(Rect::new(3, 3, 5, 5).scaled(1.5), Rect::new(5, 5, 8, 8));
        // A neighbour that starts where this one ends starts at the same pixel.
        let left = Rect::new(0, 0, 5, 5).scaled(1.5);
        let right = Rect::new(5, 0, 9, 5).scaled(1.5);
        assert_eq!(left.right, right.left);
    }

    #[test]
    fn a_rectangle_is_placed_in_its_frame() {
        let frame = Rect::new(100, 50, 1380, 770);
        let player = Rect::new(120, 150, 900, 600);
        assert_eq!(player.relative_to(frame), Rect::new(20, 100, 800, 550));
        // The frame itself is the whole window.
        assert_eq!(frame.relative_to(frame), Rect::new(0, 0, 1280, 720));
    }

    #[test]
    fn a_rectangle_partly_outside_its_frame_is_not_cut() {
        let frame = Rect::new(100, 50, 1380, 770);
        // The page scrolled: the player is partly above the top and left of the frame
        // and ends past its bottom and right.
        let player = Rect::new(60, 10, 1500, 900);
        let placed = player.relative_to(frame);
        assert_eq!(placed, Rect::new(-40, -40, 1400, 850));
        assert!(placed.left < 0 && placed.top < 0);
        assert!(placed.right > frame.right - frame.left && placed.bottom > frame.bottom - frame.top);
    }
}
