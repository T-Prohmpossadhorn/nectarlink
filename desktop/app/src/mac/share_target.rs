// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS share target: reads what `--share <target…>` carries (paths, `file://`
//! links, web links, `nectarlink://` links, or text) and the share that another
//! launch left in `activated-share.json`.
//!
//! Finder open events (`open_events`) use the same payload, through
//! [`payload_from_open_event`]. Nothing registers this as a system share
//! target, and the share picker arrives later.

use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Command-line flag to share files, URLs, or text with a paired phone.
pub const ARG: &str = "--share";

/// The file another launch leaves in the data folder for this one to take.
const ACTIVATED_FILE: &str = "activated-share.json";
/// A share is a few paths and a line of text. A bigger file is not one.
const MAX_ACTIVATED_BYTES: u64 = 1024 * 1024;

/// What a share carries: files, one link, and some text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedPayload {
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

impl SharedPayload {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.url.is_none() && self.text.is_none()
    }
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Whether `s` is a link to send as a link: web, map or `nectarlink://`.
fn is_share_url(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 4096 || s.chars().any(char::is_whitespace) {
        return false;
    }
    starts_with_ignore_case(s, "http://")
        || starts_with_ignore_case(s, "https://")
        || (starts_with_ignore_case(s, "geo:") && s.len() > "geo:".len())
        || (starts_with_ignore_case(s, "nectarlink:") && s.len() > "nectarlink:".len())
}

/// Whether `s` names a file: one that exists, or an absolute or `./` or `../`
/// path. A backslash or a `C:` start means nothing on a Mac, and a word with
/// a `/` in it, such as "and/or", is text.
fn looks_like_path(s: &str) -> bool {
    Path::new(s).exists() || s.starts_with('/') || s.starts_with("./") || s.starts_with("../")
}

/// The local path of a `file://` link, or `None` when `s` is not one. A link
/// to another computer is not a local file.
fn parse_file_uri(s: &str) -> Option<PathBuf> {
    if !starts_with_ignore_case(s, "file://") {
        return None;
    }
    let rest = &s["file://".len()..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    let (host, path) = rest.split_at(host_end);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) || path.is_empty() {
        return None;
    }
    let bytes = percent_decode(path);
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((high << 4) | low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Classifies `--share <target…>` CLI arguments into files, a link, or text.
pub fn parse_targets(targets: &[String]) -> SharedPayload {
    let mut payload = SharedPayload::default();
    let mut text_parts = Vec::new();
    for raw in targets {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(path) = parse_file_uri(trimmed) {
            payload.files.push(path);
        } else if is_share_url(trimmed) {
            if payload.url.is_none() {
                payload.url = Some(trimmed.to_owned());
            } else {
                text_parts.push(trimmed.to_owned());
            }
        } else if looks_like_path(trimmed) {
            payload.files.push(PathBuf::from(trimmed));
        } else {
            text_parts.push(trimmed.to_owned());
        }
    }
    if !text_parts.is_empty() {
        payload.text = Some(text_parts.join(" "));
    }
    payload
}

/// The payload for the URLs of one Finder open event (files opened with the
/// app or dropped on its Dock icon), or `None` when nothing in them is a
/// share. Only a `file://` link to this Mac counts. Any other link, a
/// `nectarlink://` link included, is not a share: the phone cannot take a
/// `nectarlink:` link, and no other link is something the system meant to
/// open here, so none of them becomes a link or text sent to a phone.
pub fn payload_from_open_event(urls: &[String]) -> Option<SharedPayload> {
    let kept: Vec<String> = urls.iter().filter(|url| parse_file_uri(url.trim()).is_some()).cloned().collect();
    let payload = parse_targets(&kept);
    (!payload.is_empty()).then_some(payload)
}

/// Takes the share that another launch left in `data_dir`, if there is one.
/// The file is removed whether or not it can be read, so a bad one is not
/// tried again. A link or an oversized file is removed and ignored.
pub fn take_activated_share(data_dir: &Path) -> Option<SharedPayload> {
    let path = data_dir.join(ACTIVATED_FILE);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_ACTIVATED_BYTES {
        // Not a plain file of a sane size. Remove the entry itself, never what a link points at.
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    let payload: SharedPayload = serde_json::from_slice(&bytes.ok()?).ok()?;
    (!payload.is_empty()).then_some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn file_links_become_paths() {
        let payload = parse_targets(&targets(&[
            "file:///Users/alice/My%20Documents/report%20final.pdf",
            "FILE://localhost/tmp/photo%2B1.png",
            "file:///tmp/100%25",
            "file:///tmp/bad%zzescape",
        ]));
        assert_eq!(
            payload.files,
            vec![
                PathBuf::from("/Users/alice/My Documents/report final.pdf"),
                PathBuf::from("/tmp/photo+1.png"),
                PathBuf::from("/tmp/100%"),
                PathBuf::from("/tmp/bad%zzescape"),
            ]
        );
        assert!(payload.url.is_none());
        assert!(payload.text.is_none());
    }

    #[test]
    fn file_link_decodes_bytes_that_are_not_utf8_without_loss() {
        let payload = parse_targets(&targets(&["file:///tmp/%ff%fe"]));
        assert_eq!(payload.files, vec![PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe".to_vec()))]);
    }

    #[test]
    fn file_link_to_another_computer_or_with_a_nul_is_not_a_file() {
        for link in ["file://server/share/a.txt", "file://", "file://localhost", "file:///tmp/a%00b"] {
            let payload = parse_targets(&targets(&[link]));
            assert!(payload.files.is_empty(), "{link}");
        }
        // It falls through to text instead of vanishing.
        assert_eq!(
            parse_targets(&targets(&["file://server/share/a.txt"])).text.as_deref(),
            Some("file://server/share/a.txt")
        );
    }

    #[test]
    fn links_text_and_the_nectarlink_scheme() {
        let payload = parse_targets(&targets(&[
            "https://example.com/watch?v=42",
            "Meet me at 7pm",
            "https://example.com/second",
        ]));
        assert!(payload.files.is_empty());
        assert_eq!(payload.url.as_deref(), Some("https://example.com/watch?v=42"));
        // Only one link travels as a link. The next one is kept as text.
        assert_eq!(payload.text.as_deref(), Some("Meet me at 7pm https://example.com/second"));

        let payload = parse_targets(&targets(&["nectarlink://share?text=hello"]));
        assert_eq!(payload.url.as_deref(), Some("nectarlink://share?text=hello"));
        let payload = parse_targets(&targets(&["geo:48.85,2.35"]));
        assert_eq!(payload.url.as_deref(), Some("geo:48.85,2.35"));
        // A scheme with nothing after it is text.
        let payload = parse_targets(&targets(&["nectarlink:", "geo:"]));
        assert!(payload.url.is_none());
        assert_eq!(payload.text.as_deref(), Some("nectarlink: geo:"));
    }

    #[test]
    fn paths_follow_mac_rules_not_windows_rules() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("note.txt");
        std::fs::write(&existing, b"x").unwrap();
        let existing = existing.to_string_lossy().into_owned();

        let payload = parse_targets(&targets(&[&existing, "/var/tmp/missing.tar.gz", "./a", "../b"]));
        assert_eq!(
            payload.files,
            vec![
                PathBuf::from(&existing),
                PathBuf::from("/var/tmp/missing.tar.gz"),
                PathBuf::from("./a"),
                PathBuf::from("../b")
            ]
        );
        assert!(payload.text.is_none());

        // None of these is a file on a Mac.
        let payload =
            parse_targets(&targets(&[r"C:\Users\Alice\photo.jpg", r"D:\notes.txt", "and/or", "x\\y"]));
        assert!(payload.files.is_empty());
        assert_eq!(payload.text.as_deref(), Some(r"C:\Users\Alice\photo.jpg D:\notes.txt and/or x\y"));
    }

    #[test]
    fn open_event_files_become_one_payload() {
        let payload = payload_from_open_event(&targets(&[
            "file:///Users/alice/My%20Documents/a.pdf",
            "file:///tmp/b.png",
        ]))
        .unwrap();
        assert_eq!(
            payload.files,
            vec![PathBuf::from("/Users/alice/My Documents/a.pdf"), PathBuf::from("/tmp/b.png")]
        );
        assert!(payload.url.is_none());
        assert!(payload.text.is_none());
    }

    #[test]
    fn open_event_nectarlink_link_is_not_a_share_and_empty_input_is_nothing() {
        // The phone cannot take a nectarlink: link, so it is never sent.
        assert!(payload_from_open_event(&targets(&["nectarlink://share?text=hello"])).is_none());
        let payload = payload_from_open_event(&targets(&[
            "nectarlink://pair?code=1",
            "nectarlink://share?text=hello",
            "file:///tmp/d.txt",
        ]))
        .unwrap();
        assert_eq!(payload.files, vec![PathBuf::from("/tmp/d.txt")]);
        assert!(payload.url.is_none());
        assert!(payload.text.is_none());
        assert!(payload_from_open_event(&[]).is_none());
        assert!(payload_from_open_event(&targets(&["", "  "])).is_none());
    }

    #[test]
    fn open_event_ignores_what_is_not_a_file() {
        // Other schemes, plain text, paths without a scheme and other computers
        // are never shared.
        let none = payload_from_open_event(&targets(&[
            "https://example.com",
            "geo:48.85,2.35",
            "hello world",
            "/tmp/plain-path",
            "file://server/share/a.txt",
            "nectarlink://share?text=hello",
            "nectarlink:",
        ]));
        assert!(none.is_none());
        // The good ones in the same event still go through.
        let payload =
            payload_from_open_event(&targets(&["https://example.com", "file:///tmp/c.txt"])).unwrap();
        assert_eq!(payload.files, vec![PathBuf::from("/tmp/c.txt")]);
        assert!(payload.url.is_none());
        assert!(payload.text.is_none());
    }

    #[test]
    fn nothing_to_share_is_empty() {
        assert!(parse_targets(&[]).is_empty());
        assert!(parse_targets(&targets(&["", "   "])).is_empty());
    }

    #[test]
    fn payload_round_trips_through_json_and_accepts_missing_fields() {
        let payload = SharedPayload {
            files: vec![PathBuf::from("/tmp/shared.txt")],
            url: Some("https://nectarlink.dev".into()),
            text: Some("Hello".into()),
        };
        let json = serde_json::to_vec(&payload).unwrap();
        assert_eq!(serde_json::from_slice::<SharedPayload>(&json).unwrap(), payload);
        let partial: SharedPayload = serde_json::from_str(r#"{"url":"https://a.example"}"#).unwrap();
        assert_eq!(partial.url.as_deref(), Some("https://a.example"));
        assert!(partial.files.is_empty());
    }

    #[test]
    fn take_activated_share_reads_once_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACTIVATED_FILE);
        let payload = SharedPayload {
            files: vec![PathBuf::from("/tmp/shared.txt")],
            url: Some("https://nectarlink.dev".into()),
            text: Some("Hello".into()),
        };
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        assert_eq!(take_activated_share(dir.path()), Some(payload));
        assert!(!path.exists());
        assert_eq!(take_activated_share(dir.path()), None);
    }

    #[test]
    fn take_activated_share_ignores_missing_corrupt_and_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACTIVATED_FILE);
        assert_eq!(take_activated_share(dir.path()), None);
        for content in [&b"not json"[..], b"", b"{}", br#"{"files":[],"url":null,"text":null}"#] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(take_activated_share(dir.path()), None);
            assert!(!path.exists(), "a bad file is removed");
        }
    }

    #[test]
    fn take_activated_share_refuses_links_and_big_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACTIVATED_FILE);

        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, br#"{"text":"from a link"}"#).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(take_activated_share(dir.path()), None);
        assert!(std::fs::symlink_metadata(&path).is_err(), "the link is removed");
        assert!(target.exists(), "what the link pointed at is kept");

        let big = format!(r#"{{"text":"{}"}}"#, "a".repeat(usize::try_from(MAX_ACTIVATED_BYTES).unwrap()));
        std::fs::write(&path, big).unwrap();
        assert_eq!(take_activated_share(dir.path()), None);
        assert!(!path.exists());
    }
}
