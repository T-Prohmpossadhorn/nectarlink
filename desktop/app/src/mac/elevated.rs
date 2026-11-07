// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS elevated execution: not supported. Nothing on the Mac calls this (the
//! updater offers no Mac update and setup is a Windows installer), so `start`
//! always fails.

use std::{fmt, path::Path};

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    #[allow(dead_code)]
    Declined,
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Declined => f.write_str("elevation was declined"),
            Error::Other(reason) => f.write_str(reason),
        }
    }
}

pub struct Process;

impl Process {
    /// Never `Some(0)`: this stub ran nothing, so it must not look like success.
    pub fn exit_code(&self) -> Option<u32> {
        Some(1)
    }
}

pub fn start(_program: &Path, _params: &str) -> Result<Process, Error> {
    Err(Error::Other("elevated execution is not supported on macOS".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_never_reports_success() {
        assert_ne!(Process.exit_code(), Some(0));
        assert!(start(Path::new("/bin/true"), "").is_err());
    }

    #[test]
    fn errors_display() {
        assert_eq!(Error::Declined.to_string(), "elevation was declined");
        assert_eq!(Error::Other("no".into()).to_string(), "no");
    }
}
