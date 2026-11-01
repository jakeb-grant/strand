//! (keyboard) The patterns a config claims keys with (`on key "ctrl+n",
//! "down" { … }`; spec.md 6.7): the checker parses them, logic sets a
//! node's active ones as [`crate::Prop::KeyClaims`], and the router matches
//! them against each key, all through the one type here. The named keys
//! are a static table, since the compiler builds to wasm and takes no
//! xkbcommon.

use std::fmt;

use crate::input::KeyInput;

/// The modifiers a pattern may name.
pub const KEY_MODIFIERS: &[&str] = &["ctrl", "shift", "alt", "super"];

/// A named key: what a pattern may write (any case), and the keysym names
/// it matches (a keypad's twin goes with its key, as the router's defaults
/// take both; `Next` is xkb's own name for `Page_Down`).
type Named = (&'static [&'static str], &'static [&'static str]);

/// The table: aliased keys, then those written by their own keysym name.
macro_rules! named {
    ($($entry:expr,)* ; $($name:literal),* $(,)?) => {
        &[$($entry,)* $((&[$name], &[$name]),)*]
    };
}

const NAMED: &[Named] = named![
    (&["esc", "escape"], &["Escape"]),
    (&["enter"], &["Return", "KP_Enter"]),
    (&["tab"], &["Tab", "KP_Tab", "ISO_Left_Tab"]),
    (&["space"], &["space", "KP_Space"]),
    (&["up"], &["Up", "KP_Up"]),
    (&["down"], &["Down", "KP_Down"]),
    (&["left"], &["Left", "KP_Left"]),
    (&["right"], &["Right", "KP_Right"]),
    (&["home"], &["Home", "KP_Home"]),
    (&["end"], &["End", "KP_End"]),
    (
        &["pgup", "page_up", "prior"],
        &["Page_Up", "Prior", "KP_Page_Up", "KP_Prior"],
    ),
    (
        &["pgdn", "page_down", "next"],
        &["Page_Down", "Next", "KP_Page_Down", "KP_Next"],
    ),
    (&["backspace"], &["BackSpace"]),
    (&["del", "delete"], &["Delete", "KP_Delete"]),
    (&["ins", "insert"], &["Insert", "KP_Insert"]),
    ;
    "Return", "KP_Enter", "ISO_Left_Tab", "Menu", "Print", "Pause", "Scroll_Lock", "Caps_Lock",
        "Num_Lock", "Shift_L", "Shift_R", "Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L",
        "Super_R", "Meta_L", "Meta_R", "ISO_Level3_Shift", "F1", "F2", "F3", "F4", "F5", "F6",
        "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16", "F17", "F18", "F19",
        "F20", "F21", "F22", "F23", "F24", "F25", "F26", "F27", "F28", "F29", "F30", "F31", "F32",
        "F33", "F34", "F35", "KP_0", "KP_1", "KP_2", "KP_3", "KP_4", "KP_5", "KP_6", "KP_7",
        "KP_8", "KP_9", "KP_Add", "KP_Subtract", "KP_Multiply", "KP_Divide", "KP_Decimal",
        "KP_Separator", "KP_Equal", "Multi_key", "Hyper_L", "Hyper_R", "Shift_Lock", "Mode_switch", "ISO_Level5_Shift",
    "ISO_Next_Group", "ISO_Prev_Group", "Begin", "KP_Begin", "Undo", "Redo", "Help", "Find",
    "Cancel", "Select", "Execute", "Break", "Sys_Req", "Clear", "Linefeed", "KP_F1", "KP_F2",
    "KP_F3", "KP_F4", "Muhenkan", "Henkan", "Hiragana", "Katakana", "Hangul",
    "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand", "apostrophe",
        "parenleft", "parenright", "asterisk", "plus", "comma", "minus", "period", "slash",
        "colon", "semicolon", "less", "equal", "greater", "question", "at", "bracketleft",
        "backslash", "bracketright", "asciicircum", "underscore", "grave", "braceleft", "bar",
        "braceright", "asciitilde",
];

/// What keys a character of ASCII is called when it types none: Ctrl+/
/// types a control character, which `seat.rs` drops, and a non-Latin
/// layout's key has its Latin name only in [`KeyInput::latin`].
const CHARACTERS: &[(char, &str)] = &[
    (' ', "space"),
    ('!', "exclam"),
    ('"', "quotedbl"),
    ('#', "numbersign"),
    ('$', "dollar"),
    ('%', "percent"),
    ('&', "ampersand"),
    ('\'', "apostrophe"),
    ('(', "parenleft"),
    (')', "parenright"),
    ('*', "asterisk"),
    ('+', "plus"),
    (',', "comma"),
    ('-', "minus"),
    ('.', "period"),
    ('/', "slash"),
    (':', "colon"),
    (';', "semicolon"),
    ('<', "less"),
    ('=', "equal"),
    ('>', "greater"),
    ('?', "question"),
    ('@', "at"),
    ('[', "bracketleft"),
    ('\\', "backslash"),
    (']', "bracketright"),
    ('^', "asciicircum"),
    ('_', "underscore"),
    ('`', "grave"),
    ('{', "braceleft"),
    ('|', "bar"),
    ('}', "braceright"),
    ('~', "asciitilde"),
];

/// Key names taken as written, unchecked: their families are too long and
/// too open for a table (`XF86AudioMedia`, `dead_acute`).
const OPEN_PREFIXES: &[&str] = &["xf86", "dead_"];

/// Whether `key` starts with one of [`OPEN_PREFIXES`], in any case, and goes
/// on as a keysym name does.
fn open_name(key: &str) -> bool {
    OPEN_PREFIXES.iter().any(|p| {
        key.get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
            && key.len() > p.len()
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Every name a pattern may give a key, in lower case: what the checker
/// suggests from.
pub fn key_names() -> impl Iterator<Item = String> {
    NAMED
        .iter()
        .flat_map(|(names, _)| names.iter())
        .map(|n| n.to_ascii_lowercase())
}

/// The key part of a [`KeyPattern`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum PatternKey {
    /// A letter, in lower case: the key's name or the Latin name it falls
    /// back on, in either case, or the letter it types (a non-Latin
    /// layout's keysym is not called by its letter).
    Letter(char),
    /// Any other single character: the text the key types, whatever Shift
    /// it took to type it.
    Character(char),
    /// One of the table's named keys.
    Named(&'static [&'static str]),
    /// A name of an open family ([`OPEN_PREFIXES`]), as written.
    Open(String),
}

/// A pattern that does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPatternError {
    /// Nothing, or modifiers with no key (`""`, `"ctrl+"`).
    NoKey,
    /// A modifier that is not one of [`KEY_MODIFIERS`].
    UnknownModifier(String),
    /// A name of several characters that is not in the table.
    UnknownKey(String),
    /// A modifier named twice.
    DuplicateModifier(String),
    /// A `+` that joins nothing (`"++"`, `"+a"`, `"ctrl++a"`).
    StrayPlus,
    /// `shift` on a single non-letter character (`"shift+1"`), which
    /// matches what the key types: with Shift, another character.
    ShiftOnCharacter(char),
}

impl fmt::Display for KeyPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyPatternError::NoKey => write!(f, "this key pattern names no key"),
            KeyPatternError::UnknownModifier(m) => write!(f, "`{m}` is not a modifier"),
            KeyPatternError::UnknownKey(k) => write!(f, "`{k}` is not a key"),
            KeyPatternError::DuplicateModifier(m) => write!(f, "`{m}` is named twice"),
            KeyPatternError::StrayPlus => write!(f, "this `+` joins nothing"),
            KeyPatternError::ShiftOnCharacter(c) => {
                write!(f, "`shift+{c}` is not a key: `{c}` is what a key types")
            }
        }
    }
}

/// One pattern: `ctrl+n`, `shift+tab`, `down`, `?`. Exact on its modifiers,
/// except that a single non-letter character ignores Shift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPattern {
    ctrl: bool,
    shift: bool,
    alt: bool,
    logo: bool,
    key: PatternKey,
    /// As written, for the inspector and diagnostics.
    written: String,
}

impl KeyPattern {
    /// Reads a pattern: modifiers joined with `+` before a key, in any case.
    pub fn parse(src: &str) -> Result<KeyPattern, KeyPatternError> {
        // `+` is a key too: `"+"` and `"ctrl++"`.
        let (mods, key) = match src.rsplit_once('+') {
            None => ("", src),
            Some(("", "")) => ("", "+"),
            Some((m, "")) => match m.strip_suffix('+') {
                Some("") => return Err(KeyPatternError::StrayPlus),
                Some(m) => (m, "+"),
                None => return Err(KeyPatternError::NoKey),
            },
            Some(("", _)) => return Err(KeyPatternError::StrayPlus),
            Some((m, k)) => (m, k),
        };
        let mut pattern = KeyPattern {
            ctrl: false,
            shift: false,
            alt: false,
            logo: false,
            key: PatternKey::Named(&[]),
            written: src.to_string(),
        };
        if !mods.is_empty() {
            for m in mods.split('+') {
                let held = match m.to_ascii_lowercase().as_str() {
                    "" => return Err(KeyPatternError::StrayPlus),
                    "ctrl" => &mut pattern.ctrl,
                    "shift" => &mut pattern.shift,
                    "alt" => &mut pattern.alt,
                    "super" => &mut pattern.logo,
                    _ => return Err(KeyPatternError::UnknownModifier(m.to_string())),
                };
                if std::mem::replace(held, true) {
                    return Err(KeyPatternError::DuplicateModifier(m.to_string()));
                }
            }
        }
        let mut chars = key.chars();
        pattern.key = match (chars.next(), chars.next()) {
            (None, _) => return Err(KeyPatternError::NoKey),
            (Some(c), None) if c.is_alphabetic() => {
                PatternKey::Letter(c.to_lowercase().next().unwrap_or(c))
            }
            (Some(c), None) if pattern.shift => return Err(KeyPatternError::ShiftOnCharacter(c)),
            (Some(c), None) => PatternKey::Character(c),
            _ if open_name(key) => PatternKey::Open(key.to_string()),
            _ => NAMED
                .iter()
                .find(|(names, _)| names.iter().any(|n| n.eq_ignore_ascii_case(key)))
                .map(|(_, keys)| PatternKey::Named(keys))
                .ok_or_else(|| KeyPatternError::UnknownKey(key.to_string()))?,
        };
        Ok(pattern)
    }

    /// Whether `key` is this pattern: by the key's name, then, when the
    /// pattern holds Ctrl, Alt or Super, its Latin fallback, as an input's
    /// shortcuts do: `ctrl+n` works on a Russian layout, and a plain `j`
    /// leaves the Cyrillic letter on that key to be typed.
    pub fn matches(&self, key: &KeyInput) -> bool {
        let m = key.modifiers;
        if m.ctrl != self.ctrl || m.alt != self.alt || m.logo != self.logo {
            return false;
        }
        let named = |want: &str| {
            key.name.eq_ignore_ascii_case(want)
                || ((self.ctrl || self.alt || self.logo)
                    && key
                        .latin
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(want)))
        };
        // `ISO_Left_Tab` is what Shift+Tab is called: it says Shift itself.
        let shift = m.shift || key.name == "ISO_Left_Tab";
        match &self.key {
            PatternKey::Letter(c) => {
                let mut buf = [0; 4];
                let letter = &*c.encode_utf8(&mut buf);
                shift == self.shift && (named(letter) || key.text.to_lowercase() == letter)
            }
            PatternKey::Character(c) => {
                let mut buf = [0; 4];
                let character = &*c.encode_utf8(&mut buf);
                key.text == character
                    || named(character)
                    || CHARACTERS
                        .iter()
                        .find(|(ch, _)| ch == c)
                        .is_some_and(|(_, name)| named(name))
            }
            PatternKey::Named(keys) => shift == self.shift && keys.iter().any(|k| named(k)),
            PatternKey::Open(name) => shift == self.shift && named(name),
        }
    }

    /// Whether the key types a character into an `input`: claimed around a
    /// password, it would eat what the user types there.
    pub fn types_text(&self) -> bool {
        !self.ctrl
            && !self.alt
            && !self.logo
            && match &self.key {
                PatternKey::Letter(_) | PatternKey::Character(_) => true,
                PatternKey::Named(keys) => keys.contains(&"space"),
                PatternKey::Open(_) => false,
            }
    }

    /// The pattern as written.
    pub fn written(&self) -> &str {
        &self.written
    }
}

/// One active claim of a node (a value of [`crate::Prop::KeyClaims`]): the
/// handler logic runs for it, by its number on that node, and its patterns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyClaim {
    pub handler: u32,
    pub patterns: Vec<KeyPattern>,
}

/// The handler `key` goes to among a node's active `claims`, which are in
/// source order: the last written wins, as among `when`s.
pub fn claiming(claims: &[KeyClaim], key: &KeyInput) -> Option<u32> {
    claims
        .iter()
        .rev()
        .find(|c| c.patterns.iter().any(|p| p.matches(key)))
        .map(|c| c.handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{ButtonState, Modifiers};

    fn press(name: &str, text: &str, mods: &str) -> KeyInput {
        KeyInput {
            name: name.into(),
            text: text.into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers {
                ctrl: mods.contains('c'),
                shift: mods.contains('s'),
                alt: mods.contains('a'),
                logo: mods.contains('l'),
            },
            time: 0,
            latin: None,
        }
    }

    fn pat(s: &str) -> KeyPattern {
        KeyPattern::parse(s).unwrap()
    }

    #[test]
    fn modifiers_are_exact_and_any_case() {
        let p = pat("CTRL+n");
        assert!(p.matches(&press("n", "", "c")));
        assert!(!p.matches(&press("n", "n", "")), "no ctrl");
        assert!(!p.matches(&press("N", "", "cs")), "shift too");
        assert!(!p.matches(&press("n", "", "ca")), "alt too");
        assert!(pat("ctrl+shift+alt+super+x").matches(&press("X", "", "csal")));
        assert_eq!(pat("Ctrl+N").written(), "Ctrl+N");
    }

    #[test]
    fn a_letter_says_whether_shift_is_held() {
        // xkb reports `A` with Shift.
        assert!(pat("a").matches(&press("a", "a", "")));
        assert!(!pat("a").matches(&press("A", "A", "s")));
        assert!(pat("shift+a").matches(&press("A", "A", "s")));
        assert!(!pat("shift+a").matches(&press("a", "a", "")));
        // Caps Lock reports `A` with no Shift.
        assert!(pat("a").matches(&press("A", "A", "")));
        assert!(pat("A").matches(&press("a", "a", "")), "any case");
    }

    #[test]
    fn a_character_matches_its_text_and_ignores_shift() {
        assert!(pat("?").matches(&press("question", "?", "s")));
        assert!(pat("?").matches(&press("question", "?", "")));
        assert!(pat("1").matches(&press("1", "1", "")));
        assert!(!pat("1").matches(&press("2", "2", "")));
        // On an AZERTY layout, Shift types the digit.
        assert!(pat("1").matches(&press("1", "1", "s")));
        assert!(!pat("?").matches(&press("question", "?", "c")), "ctrl held");
        assert!(pat("+").matches(&press("plus", "+", "s")));
        assert!(pat("ctrl++").matches(&press("plus", "", "cs")));
        // Ctrl+/ types a control character, which seat.rs drops.
        assert!(pat("ctrl+/").matches(&press("slash", "", "c")));
    }

    #[test]
    fn named_keys_and_their_aliases() {
        for (p, name) in [
            ("esc", "Escape"),
            ("Escape", "Escape"),
            ("ESCAPE", "Escape"),
            ("enter", "Return"),
            ("enter", "KP_Enter"),
            ("Return", "Return"),
            ("tab", "Tab"),
            ("space", "space"),
            ("up", "Up"),
            ("down", "KP_Down"),
            ("left", "Left"),
            ("right", "Right"),
            ("pgup", "Page_Up"),
            ("pgup", "Prior"),
            ("pgdn", "Next"),
            ("Page_Down", "Page_Down"),
            ("backspace", "BackSpace"),
            ("del", "Delete"),
            ("F5", "F5"),
            ("f12", "F12"),
        ] {
            assert!(pat(p).matches(&press(name, "", "")), "{p} is {name}");
        }
        assert!(!pat("Return").matches(&press("KP_Enter", "", "")));
        assert!(!pat("down").matches(&press("Up", "", "")));
    }

    #[test]
    fn shift_tab_is_iso_left_tab() {
        assert!(pat("shift+tab").matches(&press("ISO_Left_Tab", "", "s")));
        assert!(pat("shift+tab").matches(&press("Tab", "", "s")));
        // A compositor's keymap may send it with no Shift in the mask.
        assert!(pat("shift+tab").matches(&press("ISO_Left_Tab", "", "")));
        assert!(!pat("tab").matches(&press("ISO_Left_Tab", "", "s")));
        assert!(!pat("shift+tab").matches(&press("Tab", "", "")));
    }

    #[test]
    fn the_latin_fallback_matches_a_non_latin_layout() {
        let mut key = press("Cyrillic_te", "", "c");
        key.latin = Some("n".into());
        assert!(pat("ctrl+n").matches(&key));
        assert!(!pat("ctrl+m").matches(&key));
        assert!(pat("alt+n").matches(&KeyInput {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            ..key.clone()
        }));
        // With no Ctrl, Alt or Super, the key is the Cyrillic letter it types.
        let mut ya = press("Cyrillic_ya", "я", "");
        ya.latin = Some("z".into());
        assert!(pat("я").matches(&ya));
        assert!(
            !pat("z").matches(&ya),
            "a plain letter leaves it to be typed"
        );
        let mut te = press("Cyrillic_TE", "Т", "s");
        te.latin = Some("n".into());
        assert!(!pat("shift+n").matches(&te), "Shift alone is plain too");
        // Punctuation's Latin name.
        let mut dot = press("Cyrillic_yu", "ю", "c");
        dot.latin = Some("period".into());
        assert!(pat("ctrl+.").matches(&dot));
        dot.modifiers.ctrl = false;
        assert!(!pat(".").matches(&dot));
    }

    #[test]
    fn bad_patterns_say_what_is_wrong() {
        use KeyPatternError::*;
        assert_eq!(KeyPattern::parse(""), Err(NoKey));
        assert_eq!(KeyPattern::parse("ctrl+"), Err(NoKey));
        assert_eq!(
            KeyPattern::parse("ctl+n"),
            Err(UnknownModifier("ctl".into()))
        );
        assert_eq!(
            KeyPattern::parse("ctrl+escpe"),
            Err(UnknownKey("escpe".into()))
        );
        assert!(key_names().any(|n| n == "page_down"));
        assert_eq!(
            KeyPattern::parse("ctrl+ctrl+n"),
            Err(DuplicateModifier("ctrl".into()))
        );
        assert_eq!(
            KeyPattern::parse("Ctrl+ctrl+n"),
            Err(DuplicateModifier("ctrl".into()))
        );
        for stray in ["++", "+a", "ctrl++a"] {
            assert_eq!(KeyPattern::parse(stray), Err(StrayPlus), "{stray}");
        }
        assert_eq!(KeyPattern::parse("shift+1"), Err(ShiftOnCharacter('1')));
        assert_eq!(
            KeyPattern::parse("ctrl+shift+?"),
            Err(ShiftOnCharacter('?'))
        );
        // `+` stays a key, and Shift a letter's or a named key's.
        assert!(KeyPattern::parse("+").is_ok() && KeyPattern::parse("ctrl++").is_ok());
        assert!(KeyPattern::parse("shift+a").is_ok() && KeyPattern::parse("shift+f5").is_ok());
    }

    #[test]
    fn open_families_and_the_common_names_are_keys() {
        for (p, name) in [
            ("XF86AudioMedia", "XF86AudioMedia"),
            ("xf86wlan", "XF86WLAN"),
            ("XF86KbdBrightnessUp", "XF86KbdBrightnessUp"),
            ("dead_acute", "dead_acute"),
            ("Multi_key", "Multi_key"),
            ("Hyper_L", "Hyper_L"),
            ("Begin", "Begin"),
            ("Undo", "Undo"),
            ("Help", "Help"),
        ] {
            assert!(pat(p).matches(&press(name, "", "")), "{p}");
            assert!(!pat(p).matches(&press("a", "a", "")), "{p}");
            assert!(!pat(p).types_text(), "{p}");
        }
        assert!(pat("ctrl+XF86Search").matches(&press("XF86Search", "", "c")));
        // The prefix alone, or what no keysym is called, is not one.
        for bad in ["XF86", "dead_", "XF86 Audio"] {
            assert!(KeyPattern::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_last_written_active_claim_wins() {
        let claims = [
            KeyClaim {
                handler: 0,
                patterns: vec![pat("a"), pat("b")],
            },
            KeyClaim {
                handler: 3,
                patterns: vec![pat("b")],
            },
        ];
        assert_eq!(claiming(&claims, &press("a", "a", "")), Some(0));
        assert_eq!(claiming(&claims, &press("b", "b", "")), Some(3));
        assert_eq!(claiming(&claims, &press("c", "c", "")), None);
    }

    #[test]
    fn what_types_text_is_known() {
        for p in ["a", "shift+a", "?", "1", "space"] {
            assert!(pat(p).types_text(), "{p}");
        }
        for p in ["ctrl+a", "alt+1", "super+x", "enter", "tab", "backspace"] {
            assert!(!pat(p).types_text(), "{p}");
        }
    }
}
