// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS tasks: the actions a menu entry or a widget button asks for, with
//! their titles. Nothing shows them yet: the Dock menu will.

use std::path::Path;

use crate::i18n::tr;

pub const ARG: &str = "--task";

/// The tasks, in menu order. Each one is an action that
/// `bridge::app::handle_jump_task` runs.
// Unused until the Dock menu lists them.
#[cfg_attr(not(test), allow(dead_code))]
pub const TASKS: &[&str] = &["send-files", "send-clipboard", "find-phone", "take-photo", "open-timeline"];

/// The title of a task in the app's language, or `None` for any other action.
// Unused until the Dock menu lists them.
#[cfg_attr(not(test), allow(dead_code))]
pub fn title(action: &str) -> Option<String> {
    Some(match action {
        "send-files" => tr!("App", "Send files"),
        "send-clipboard" => tr!("App", "Send clipboard"),
        "find-phone" => tr!("App", "Find my phone"),
        "take-photo" => tr!("App", "Take photo with phone"),
        "open-timeline" => tr!("App", "Open Timeline"),
        _ => return None,
    })
}

pub fn register(_data_dir: Option<&Path>) {}

pub fn unregister() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_keep_their_order_and_all_have_a_title() {
        assert_eq!(TASKS, ["send-files", "send-clipboard", "find-phone", "take-photo", "open-timeline"]);
        for action in TASKS {
            assert!(title(action).is_some_and(|t| !t.is_empty()), "{action}");
        }
        assert_eq!(title("open-shelf"), None);
        assert_eq!(title(""), None);
    }

    #[test]
    fn every_task_has_a_branch_in_the_dispatcher() {
        // The dispatcher is a closure over Qt objects, so this reads its source
        // instead of running it.
        let source = include_str!("../bridge/app.rs");
        let start = source.find("fn dispatch_jump_task").expect("dispatcher exists");
        let body = &source[start..];
        let body = &body[..body.find("\nfn ").unwrap_or(body.len())];
        for action in TASKS {
            assert!(body.contains(&format!("\"{action}\" =>")), "no branch for {action}");
        }
    }
}
