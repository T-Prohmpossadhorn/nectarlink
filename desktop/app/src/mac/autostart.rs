// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS login item / autostart via LaunchAgents.
//!
//! Error texts here never hold the program path, because callers log them.

use std::fs;
use std::io;
use std::path::Path;

/// The LaunchAgent label. It also names the plist file.
const LABEL: &str = "com.thtbee.nectarlink";

fn plist_name() -> String {
    format!("{LABEL}.plist")
}

/// The file the plist is written to first, in the same folder, so that the
/// final rename stays on one volume.
fn temp_name() -> String {
    format!(".{LABEL}.plist.tmp")
}

fn agents_dir() -> Result<std::path::PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join("Library/LaunchAgents"))
        .ok_or_else(|| "can't find the home folder".to_string())
}

pub fn is_enabled() -> bool {
    agents_dir().is_ok_and(|dir| is_enabled_in(&dir))
}

fn is_enabled_in(agents: &Path) -> bool {
    agents.join(plist_name()).exists()
}

pub fn is_installed(_exe: &Path) -> bool {
    is_enabled()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The text of the LaunchAgent plist that starts `exe` at login.
///
/// It fails for a path that is not Unicode, and for one with a control
/// character, which XML cannot hold.
fn plist_text(exe: &Path) -> Result<String, String> {
    let exe = exe.to_str().ok_or("the program path is not valid Unicode")?;
    if exe.chars().any(|c| c.is_control() || c == '\u{FFFE}' || c == '\u{FFFF}') {
        return Err("the program path has a character that a plist cannot hold".to_string());
    }
    let escaped_exe = xml_escape(exe);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{escaped_exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#
    ))
}

/// Writes `content` to `plist` through a temp file, so a crash never leaves
/// a half-written plist behind.
fn write_atomic(agents: &Path, plist: &Path, content: &str) -> io::Result<()> {
    let temp = agents.join(temp_name());
    let result = fs::write(&temp, content).and_then(|()| fs::rename(&temp, plist));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Adds or removes the login item for `exe` in the LaunchAgents folder `agents`.
fn set_in(agents: &Path, enabled: bool, exe: &Path) -> Result<(), String> {
    let plist = agents.join(plist_name());
    if enabled {
        let content = plist_text(exe)?;
        fs::create_dir_all(agents).map_err(|e| format!("can't create the LaunchAgents folder: {e}"))?;
        write_atomic(agents, &plist, &content).map_err(|e| format!("can't write the login item: {e}"))
    } else {
        match fs::remove_file(&plist) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("can't remove the login item: {e}")),
        }
    }
}

pub fn set(enabled: bool, exe: &Path) -> Result<(), String> {
    set_in(&agents_dir()?, enabled, exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    fn exe_in_bundle() -> PathBuf {
        PathBuf::from("/Applications/NectarLink.app/Contents/MacOS/nectarlink-desktop")
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn plist_text_is_exact() {
        let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.thtbee.nectarlink</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/NectarLink.app/Contents/MacOS/nectarlink-desktop</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#;
        assert_eq!(plist_text(&exe_in_bundle()).unwrap(), expected);
    }

    #[test]
    fn escapes_the_characters_xml_reserves() {
        let text = plist_text(Path::new("/Apps/A&B <x> \"q\" 'r'/bin")).unwrap();
        assert!(text.contains("<string>/Apps/A&amp;B &lt;x&gt; &quot;q&quot; &apos;r&apos;/bin</string>"));
        // An ampersand is escaped once, not twice.
        assert!(!text.contains("&amp;amp;"));
        // Non-ASCII text stays as it is.
        let text = plist_text(Path::new("/Apps/Caf\u{e9} \u{1F430}/bin")).unwrap();
        assert!(text.contains("<string>/Apps/Caf\u{e9} \u{1F430}/bin</string>"));
    }

    #[test]
    fn rejects_a_path_that_is_not_unicode() {
        let exe = Path::new(OsStr::from_bytes(b"/Apps/bad\xff/bin"));
        let err = plist_text(exe).unwrap_err();
        assert!(!err.contains("Apps"));

        let dir = tempfile::tempdir().unwrap();
        assert!(set_in(dir.path(), true, exe).is_err());
        assert!(file_names(dir.path()).is_empty());
    }

    #[test]
    fn rejects_characters_that_xml_cannot_hold() {
        for bad in
            ["/Apps/a\nb", "/Apps/a\u{1}b", "/Apps/a\tb", "/Apps/a\rb", "/Apps/a\u{7f}b", "/Apps/a\u{FFFE}b"]
        {
            let err = plist_text(Path::new(bad)).unwrap_err();
            assert!(!err.contains("Apps"), "the error must not hold the path");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(set_in(dir.path(), true, Path::new("/Apps/a\nb")).is_err());
        assert!(file_names(dir.path()).is_empty());
    }

    #[test]
    fn enabling_writes_the_plist_for_the_given_exe() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_enabled_in(dir.path()));
        set_in(dir.path(), true, &exe_in_bundle()).unwrap();
        assert!(is_enabled_in(dir.path()));
        let written = fs::read_to_string(dir.path().join("com.thtbee.nectarlink.plist")).unwrap();
        assert_eq!(written, plist_text(&exe_in_bundle()).unwrap());
        assert!(written.contains("/Applications/NectarLink.app/Contents/MacOS/nectarlink-desktop"));
    }

    #[test]
    fn enabling_creates_a_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("Library/LaunchAgents");
        set_in(&agents, true, &exe_in_bundle()).unwrap();
        assert!(is_enabled_in(&agents));
    }

    #[test]
    fn enabling_fails_when_the_folder_cannot_be_made() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the folder should be.
        let blocker = dir.path().join("Library");
        fs::write(&blocker, b"").unwrap();
        let err = set_in(&blocker.join("LaunchAgents"), true, &exe_in_bundle()).unwrap_err();
        assert!(err.contains("LaunchAgents folder"));
    }

    #[test]
    fn enabling_replaces_an_old_plist_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.thtbee.nectarlink.plist");
        fs::write(&plist, "old").unwrap();
        set_in(dir.path(), true, &exe_in_bundle()).unwrap();
        assert_eq!(fs::read_to_string(&plist).unwrap(), plist_text(&exe_in_bundle()).unwrap());
        assert_eq!(file_names(dir.path()), ["com.thtbee.nectarlink.plist"]);
        // Again, with another program: the content follows the argument.
        let other = PathBuf::from("/Users/me/Apps/Other.app/Contents/MacOS/other");
        set_in(dir.path(), true, &other).unwrap();
        assert_eq!(fs::read_to_string(&plist).unwrap(), plist_text(&other).unwrap());
        assert_eq!(file_names(dir.path()), ["com.thtbee.nectarlink.plist"]);
    }

    #[test]
    fn a_failed_write_leaves_the_old_plist_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A folder where the plist should be makes the rename fail.
        let plist = dir.path().join("com.thtbee.nectarlink.plist");
        fs::create_dir(&plist).unwrap();
        let err = set_in(dir.path(), true, &exe_in_bundle()).unwrap_err();
        assert!(err.contains("can't write the login item"));
        assert_eq!(file_names(dir.path()), ["com.thtbee.nectarlink.plist"]);
        assert!(plist.is_dir());
    }

    #[test]
    fn disabling_removes_the_plist_and_is_fine_when_it_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        set_in(dir.path(), true, &exe_in_bundle()).unwrap();
        set_in(dir.path(), false, &exe_in_bundle()).unwrap();
        assert!(!is_enabled_in(dir.path()));
        // Already gone: still a success.
        set_in(dir.path(), false, &exe_in_bundle()).unwrap();
        // The folder itself is missing: still a success.
        set_in(&dir.path().join("nowhere"), false, &exe_in_bundle()).unwrap();
    }

    #[test]
    fn disabling_reports_an_error_that_is_not_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // remove_file on a folder fails, and it is not NotFound.
        fs::create_dir(dir.path().join("com.thtbee.nectarlink.plist")).unwrap();
        let err = set_in(dir.path(), false, &exe_in_bundle()).unwrap_err();
        assert!(err.contains("can't remove the login item"));
    }

    #[test]
    fn disabling_does_not_need_a_usable_exe_path() {
        let dir = tempfile::tempdir().unwrap();
        set_in(dir.path(), true, &exe_in_bundle()).unwrap();
        set_in(dir.path(), false, Path::new(OsStr::from_bytes(b"/bad\xff"))).unwrap();
        assert!(!is_enabled_in(dir.path()));
    }
}
