// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS has no Send to folder and no shortcut files, so `send_to_dir` and
//! `recent_dir` stay `None` and nothing calls `write`. The text format here
//! (target, arguments, description, one per line) is private to this module:
//! it is not a Finder alias or anything else the system understands. The Mac
//! Send to is the Services menu.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub target: PathBuf,
    pub arguments: String,
    pub description: String,
}

/// There is no Send to folder on a Mac.
pub fn send_to_dir() -> Option<PathBuf> {
    None
}

/// The recent items list of a Mac has a private format, so there is no folder to read.
pub fn recent_dir() -> Option<PathBuf> {
    None
}

pub fn arguments(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let _target = lines.next()?;
    let arguments = lines.next()?;
    Some(arguments.to_string())
}

pub fn target(path: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let target = lines.next()?;
    Some(PathBuf::from(target))
}

pub fn write(path: &Path, shortcut: &Shortcut) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!("{}\n{}\n{}\n", shortcut.target.display(), shortcut.arguments, shortcut.description),
    )
}
