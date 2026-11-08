// SPDX-License-Identifier: GPL-3.0-or-later
//! The names of modifier keys in the shortcuts the app shows and records.
//!
//! A shortcut spec is text such as `Ctrl+Alt+Space`. Two kinds of spec exist,
//! and on a Mac they use the same four words for different keys:
//!
//! - A **recorded** spec comes from a key press that Qt reported (the hotkey
//!   boxes in Settings, the `Ctrl+K` shortcut). Qt swaps Control and Command
//!   on a Mac, because `AA_MacDontSwapCtrlAndMeta` is not set. `Qt.ControlModifier`
//!   is the Command key and `Qt.MetaModifier` is the Control key. So a recorded
//!   `Ctrl` is Command and a recorded `Win` is Control.
//! - A **pressed** spec is a key press that the app makes for the user (the
//!   Deck). There `Ctrl` is the Control key, `Win` is the Command key and
//!   `Alt` is the Option key (`mac/input.rs`).
//!
//! The names are English here. [`shortcut_label`] translates the Mac names.
//! On Windows and Linux the spec is shown as it is.

use crate::i18n::tr;

/// The system the names are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    Linux,
    Mac,
}

impl Os {
    /// The system this build runs on.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Windows
        }
    }
}

/// A modifier as a spec writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Win,
}

impl Modifier {
    /// The modifier a spec word stands for (`Super` is the Linux word for `Win`).
    pub fn parse(word: &str) -> Option<Modifier> {
        match word {
            "Ctrl" => Some(Modifier::Ctrl),
            "Alt" => Some(Modifier::Alt),
            "Shift" => Some(Modifier::Shift),
            "Win" | "Super" => Some(Modifier::Win),
            _ => None,
        }
    }
}

/// Where a spec comes from (see the module text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Recorded from Qt key events: Qt has already swapped Control and Command.
    Recorded,
    /// Pressed by the app: the words name the real keys.
    Pressed,
}

/// The English name of `modifier` in a spec from `source`, on `os`.
pub fn modifier_name(os: Os, source: Source, modifier: Modifier) -> &'static str {
    match (os, source, modifier) {
        (_, _, Modifier::Shift) => "Shift",
        (Os::Mac, Source::Recorded, Modifier::Ctrl) => "Command",
        (Os::Mac, Source::Recorded, Modifier::Win) => "Control",
        (Os::Mac, Source::Pressed, Modifier::Ctrl) => "Control",
        (Os::Mac, Source::Pressed, Modifier::Win) => "Command",
        (Os::Mac, _, Modifier::Alt) => "Option",
        (_, _, Modifier::Ctrl) => "Ctrl",
        (_, _, Modifier::Alt) => "Alt",
        (Os::Linux, _, Modifier::Win) => "Super",
        (Os::Windows, _, Modifier::Win) => "Win",
    }
}

/// The modifiers held in a Qt key event. `ctrl` and `meta` are Qt's flags:
/// on a Mac `ctrl` is the Command key and `meta` is the Control key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// The modifier part of a recorded spec (`Ctrl+Alt`, no key), or `""` when
/// no modifier is held. The words and their order are the ones the hotkey
/// boxes have always stored. Only the word for `meta` depends on the system:
/// `Super` on Linux and `Win` elsewhere, a Mac included.
pub fn recorded_modifiers(os: Os, held: Held) -> String {
    let mut words: Vec<&str> = Vec::new();
    if held.ctrl {
        words.push("Ctrl");
    }
    if held.alt {
        words.push("Alt");
    }
    if held.shift {
        words.push("Shift");
    }
    if held.meta {
        words.push(if os == Os::Linux { "Super" } else { "Win" });
    }
    words.join("+")
}

/// `spec` as the user should read it. On a Mac each modifier gets its Mac
/// name, put through `translate`; anything else in the spec stays. On
/// Windows and Linux the spec is returned as it is.
pub fn shortcut_with(
    os: Os,
    spec: &str,
    source: Source,
    translate: impl Fn(&'static str) -> String,
) -> String {
    if os != Os::Mac {
        return spec.to_owned();
    }
    spec.split('+')
        .map(|word| match Modifier::parse(word) {
            Some(modifier) => translate(modifier_name(os, source, modifier)),
            None => word.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// The name of a Mac key, in the app's language.
fn translated_mac_name(name: &'static str) -> String {
    match name {
        "Command" => tr!("Shortcuts", "Command"),
        "Control" => tr!("Shortcuts", "Control"),
        "Option" => tr!("Shortcuts", "Option"),
        "Shift" => tr!("Shortcuts", "Shift"),
        other => other.to_owned(),
    }
}

/// `spec` as the user should read it on this system, in the app's language.
pub fn shortcut_label(spec: &str, source: Source) -> String {
    shortcut_with(Os::current(), spec, source, translated_mac_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english(os: Os, spec: &str, source: Source) -> String {
        shortcut_with(os, spec, source, |name| name.to_owned())
    }

    #[test]
    fn a_mac_swaps_the_recorded_words() {
        assert_eq!(modifier_name(Os::Mac, Source::Recorded, Modifier::Ctrl), "Command");
        assert_eq!(modifier_name(Os::Mac, Source::Recorded, Modifier::Win), "Control");
        assert_eq!(modifier_name(Os::Mac, Source::Recorded, Modifier::Alt), "Option");
        assert_eq!(modifier_name(Os::Mac, Source::Recorded, Modifier::Shift), "Shift");
    }

    #[test]
    fn a_mac_names_the_real_keys_the_app_presses() {
        assert_eq!(modifier_name(Os::Mac, Source::Pressed, Modifier::Ctrl), "Control");
        assert_eq!(modifier_name(Os::Mac, Source::Pressed, Modifier::Win), "Command");
        assert_eq!(modifier_name(Os::Mac, Source::Pressed, Modifier::Alt), "Option");
        assert_eq!(modifier_name(Os::Mac, Source::Pressed, Modifier::Shift), "Shift");
    }

    #[test]
    fn windows_and_linux_keep_their_names_for_both_sources() {
        for source in [Source::Recorded, Source::Pressed] {
            assert_eq!(modifier_name(Os::Windows, source, Modifier::Ctrl), "Ctrl");
            assert_eq!(modifier_name(Os::Windows, source, Modifier::Alt), "Alt");
            assert_eq!(modifier_name(Os::Windows, source, Modifier::Shift), "Shift");
            assert_eq!(modifier_name(Os::Windows, source, Modifier::Win), "Win");
            assert_eq!(modifier_name(Os::Linux, source, Modifier::Ctrl), "Ctrl");
            assert_eq!(modifier_name(Os::Linux, source, Modifier::Alt), "Alt");
            assert_eq!(modifier_name(Os::Linux, source, Modifier::Win), "Super");
        }
    }

    #[test]
    fn super_and_win_are_the_same_modifier() {
        assert_eq!(Modifier::parse("Super"), Some(Modifier::Win));
        assert_eq!(Modifier::parse("Win"), Some(Modifier::Win));
        assert_eq!(Modifier::parse("Space"), None);
        assert_eq!(Modifier::parse("ctrl"), None);
    }

    #[test]
    fn recorded_modifiers_keep_the_stored_words_and_order() {
        let all = Held { ctrl: true, alt: true, shift: true, meta: true };
        assert_eq!(recorded_modifiers(Os::Windows, all), "Ctrl+Alt+Shift+Win");
        assert_eq!(recorded_modifiers(Os::Linux, all), "Ctrl+Alt+Shift+Super");
        assert_eq!(recorded_modifiers(Os::Mac, all), "Ctrl+Alt+Shift+Win");
        assert_eq!(recorded_modifiers(Os::Mac, Held::default()), "");
        let alt_only = Held { alt: true, ..Held::default() };
        assert_eq!(recorded_modifiers(Os::Windows, alt_only), "Alt");
    }

    #[test]
    fn command_is_stored_as_ctrl_and_read_back_as_command() {
        // Command held: Qt reports the Control flag.
        let command = Held { ctrl: true, ..Held::default() };
        let spec = format!("{}+K", recorded_modifiers(Os::Mac, command));
        assert_eq!(spec, "Ctrl+K");
        assert_eq!(english(Os::Mac, &spec, Source::Recorded), "Command+K");
        // The same spec on Windows is the Control key and is shown as stored.
        assert_eq!(english(Os::Windows, &spec, Source::Recorded), "Ctrl+K");
    }

    #[test]
    fn control_is_stored_as_win_and_read_back_as_control() {
        // Control held on a Mac: Qt reports the Meta flag.
        let control = Held { alt: true, meta: true, ..Held::default() };
        let spec = format!("{}+Space", recorded_modifiers(Os::Mac, control));
        assert_eq!(spec, "Alt+Win+Space");
        assert_eq!(english(Os::Mac, &spec, Source::Recorded), "Option+Control+Space");
        assert_eq!(english(Os::Windows, &spec, Source::Recorded), "Alt+Win+Space");
        assert_eq!(english(Os::Linux, "Alt+Super+Space", Source::Recorded), "Alt+Super+Space");
    }

    #[test]
    fn pressed_specs_name_the_real_keys_on_a_mac() {
        assert_eq!(english(Os::Mac, "Win+Tab", Source::Pressed), "Command+Tab");
        assert_eq!(english(Os::Mac, "Shift+Win+5", Source::Pressed), "Shift+Command+5");
        assert_eq!(english(Os::Mac, "Ctrl", Source::Pressed), "Control");
        assert_eq!(english(Os::Windows, "Win+Shift+S", Source::Pressed), "Win+Shift+S");
    }

    #[test]
    fn only_modifier_words_are_renamed() {
        assert_eq!(english(Os::Mac, "Ctrl+Alt+Space", Source::Recorded), "Command+Option+Space");
        assert_eq!(english(Os::Mac, "Ctrl+F5", Source::Recorded), "Command+F5");
        assert_eq!(english(Os::Mac, "", Source::Recorded), "");
        assert_eq!(english(Os::Mac, "Ctrl+Shift", Source::Recorded), "Command+Shift");
    }

    #[test]
    fn the_translator_sees_each_mac_name_once() {
        let seen = std::cell::RefCell::new(Vec::new());
        let text = shortcut_with(Os::Mac, "Ctrl+Alt+Win+Shift+X", Source::Recorded, |name| {
            seen.borrow_mut().push(name);
            format!("<{name}>")
        });
        assert_eq!(text, "<Command>+<Option>+<Control>+<Shift>+X");
        assert_eq!(seen.into_inner(), vec!["Command", "Option", "Control", "Shift"]);
    }
}
