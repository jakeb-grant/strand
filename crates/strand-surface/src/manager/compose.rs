//! (keyboard) Dead keys and Compose sequences: what a key press types.
//!
//! SCTK composes too, but its state is private: it can't be reset when
//! the focus moves, and a sequence begun in one surface would finish in
//! the next. So each keyboard has its own [`KeyText`], fed before a key
//! is sent, and SCTK's `utf8` is only the answer when no table loaded,
//! which is then the key's own text.

use smithay_client_toolkit::seat::keyboard::Keysym;
use std::ffi::OsString;
use strand_scene::SurfaceId;
use xkbcommon::xkb;
use xkbcommon::xkb::compose;
use zeroize::Zeroizing;

/// The locale compose tables are looked up by: the first of `LC_ALL`,
/// `LC_CTYPE` and `LANG` that is set and not empty, else `C`.
fn locale_from(var: impl Fn(&str) -> Option<OsString>) -> OsString {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| var(name).filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".into())
}

/// The compose table of this process's locale, or `None` when it has
/// none, which is no fault: keys then type what they type. `XCOMPOSEFILE`
/// and `~/.XCompose` are libxkbcommon's to honour.
pub(super) fn locale_table() -> Option<compose::Table> {
    let locale = locale_from(|name| std::env::var_os(name));
    let mut context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    context.set_log_level(xkb::LogLevel::Critical);
    let table = compose::Table::new_from_locale(&context, &locale, compose::COMPILE_NO_FLAGS).ok();
    if table.is_none() {
        log::debug!("no compose table for the locale {locale:?}");
    }
    table
}

/// One keyboard's text state: its keymap's xkb state, kept beside SCTK's
/// so a key's own text is known whatever SCTK's compose state did with
/// it, and its compose state.
pub(super) struct KeyText {
    xkb: xkb::State,
    compose: Option<compose::State>,
    /// The surface its last press went to.
    surface: Option<SurfaceId>,
    /// The key whose press was spent, until its release is.
    consumed_release: Option<u32>,
}

impl KeyText {
    pub(super) fn new(keymap: &xkb::Keymap, table: Option<&compose::Table>) -> KeyText {
        let mut text = KeyText {
            xkb: xkb::State::new(keymap),
            compose: None,
            surface: None,
            consumed_release: None,
        };
        if let Some(table) = table {
            text.set_table(table);
        }
        text
    }

    /// Composes from `table` from now on, unless it already does.
    pub(super) fn set_table(&mut self, table: &compose::Table) {
        if self.compose.is_none() {
            self.compose = Some(compose::State::new(table, compose::STATE_NO_FLAGS));
        }
    }

    /// Forgets a sequence under way.
    pub(super) fn reset(&mut self) {
        if let Some(compose) = &mut self.compose {
            compose.reset();
        }
    }

    /// The keyboard's modifier state, as `wl_keyboard.modifiers` tells it.
    pub(super) fn update_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) {
        self.xkb
            .update_mask(depressed, latched, locked, 0, 0, group);
    }

    /// True while a sequence is under way.
    pub(super) fn pending(&self) -> bool {
        self.compose
            .as_ref()
            .is_some_and(|c| c.status() == compose::Status::Composing)
    }

    /// True once if the release of `raw` belongs to a press that was
    /// spent, so that it is not delivered either.
    pub(super) fn take_consumed_release(&mut self, raw: u32) -> bool {
        let consumed = self.consumed_release == Some(raw);
        if consumed {
            self.consumed_release = None;
        }
        consumed
    }

    /// What a press of key `raw`, keysym `sym`, types on `surface`, or
    /// `None` when the press is spent: an Escape that ended a sequence,
    /// which the host never hears of. A sequence does not follow the keys
    /// to another surface. `chord`: Ctrl, Alt or Super is held, so the key
    /// is a shortcut, no key of a sequence, and ends one. `sctk`: the text
    /// SCTK gave, which stands when no compose table loaded.
    pub(super) fn press(
        &mut self,
        surface: SurfaceId,
        raw: u32,
        sym: Keysym,
        chord: bool,
        sctk: Zeroizing<String>,
    ) -> Option<Zeroizing<String>> {
        if self.surface.replace(surface) != Some(surface) {
            self.reset();
        }
        // Its release is this press's now.
        self.take_consumed_release(raw);
        let pending = self.pending();
        let Some(compose) = &mut self.compose else {
            return Some(sctk);
        };
        // `wl_keyboard` sends evdev codes, xkb's are 8 more.
        let code = xkb::Keycode::new(raw.saturating_add(8));
        let key = Zeroizing::new(self.xkb.key_get_utf8(code));
        if chord {
            compose.reset();
            return Some(key);
        }
        if sym == Keysym::Escape {
            compose.reset();
            if pending {
                self.consumed_release = Some(raw);
                return None;
            }
            return Some(key);
        }
        Some(match compose.feed(sym) {
            // A modifier: it leaves the sequence where it was.
            compose::FeedResult::Ignored => key,
            compose::FeedResult::Accepted => match compose.status() {
                compose::Status::Composed => Zeroizing::new(compose.utf8().unwrap_or_default()),
                compose::Status::Nothing => key,
                compose::Status::Composing | compose::Status::Cancelled => {
                    Zeroizing::new(String::new())
                }
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYMAP: &str = "xkb_keymap {\n\
        xkb_keycodes \"t\" { minimum = 8; maximum = 255; <ESC> = 9; <AD03> = 26; <AD09> = 32;\n\
          <AB03> = 54; <AC11> = 48; <LFSH> = 50; <RALT> = 108; <AC01> = 38; };\n\
        xkb_types \"t\" {\n\
          type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; };\n\
          type \"TWO_LEVEL\" { modifiers = Shift; map[Shift] = Level2;\n\
            level_name[Level1] = \"Base\"; level_name[Level2] = \"Shift\"; };\n\
        };\n\
        xkb_compatibility \"t\" { };\n\
        xkb_symbols \"t\" {\n\
          key <ESC> { [ Escape ] };\n\
          key <AD03> { type = \"TWO_LEVEL\", [ e, E ] };\n\
          key <AD09> { [ o ] };\n\
          key <AB03> { [ c ] };\n\
          key <AC01> { [ a ] };\n\
          key <AC11> { [ dead_acute ] };\n\
          key <RALT> { [ Multi_key ] };\n\
          key <LFSH> { [ Shift_L ] };\n\
        };\n\
        };\n";

    const TABLE: &str = "<dead_acute> <e> : \"é\" eacute\n\
        <dead_acute> <E> : \"É\" Eacute\n\
        <Multi_key> <o> <c> : \"©\" copyright\n";

    // evdev's codes, the keymap's less 8.
    const ESC: u32 = 1;
    const E: u32 = 18;
    const O: u32 = 24;
    const A: u32 = 30;
    const DEAD: u32 = 40;
    const SHIFT: u32 = 42;
    const C: u32 = 46;
    const MULTI: u32 = 100;
    const HERE: SurfaceId = SurfaceId(1);
    const THERE: SurfaceId = SurfaceId(2);

    fn table() -> compose::Table {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        compose::Table::new_from_buffer(
            &context,
            TABLE,
            "C",
            compose::FORMAT_TEXT_V1,
            compose::COMPILE_NO_FLAGS,
        )
        .unwrap()
    }

    fn keys(table: Option<&compose::Table>) -> KeyText {
        KeyText::new(&super::super::seat::compile_keymap(KEYMAP).unwrap(), table)
    }

    /// The text of pressing `raw`, SCTK having said `sctk`.
    fn press_on(k: &mut KeyText, on: SurfaceId, raw: u32, sctk: &str) -> String {
        let sym = k.xkb.key_get_one_sym(xkb::Keycode::new(raw + 8));
        k.press(on, raw, sym, false, Zeroizing::new(sctk.to_string()))
            .expect("delivered")
            .to_string()
    }

    fn press(k: &mut KeyText, raw: u32) -> String {
        press_on(k, HERE, raw, "from sctk")
    }

    #[test]
    fn a_dead_key_and_a_letter_type_the_composed_letter() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, DEAD), "", "a dead key types nothing yet");
        assert_eq!(press(&mut k, E), "é");
        // The sequence is over: the same key types itself.
        assert_eq!(press(&mut k, E), "e");
    }

    #[test]
    fn a_compose_sequence_types_its_result_on_its_last_key() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, MULTI), "");
        assert_eq!(press(&mut k, O), "");
        assert_eq!(press(&mut k, C), "©");
        assert_eq!(press(&mut k, C), "c");
    }

    #[test]
    fn a_modifier_is_no_part_of_a_sequence_and_picks_the_result() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, DEAD), "");
        assert_eq!(press(&mut k, SHIFT), "");
        k.update_modifiers(1, 0, 0, 0);
        assert_eq!(press(&mut k, E), "É");
        assert_eq!(press(&mut k, E), "E");
    }

    /// A key that continues no sequence ends it, typing nothing, as xkb's
    /// status says, and the key after it is itself again.
    #[test]
    fn a_cancelled_sequence_types_nothing_and_is_over() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, DEAD), "");
        assert_eq!(press(&mut k, A), "");
        assert_eq!(press(&mut k, A), "a");
        assert_eq!(press(&mut k, E), "e");
    }

    /// An Escape that ends a sequence is spent, its release too, and the
    /// sequence is over; one with nothing pending is an ordinary key.
    #[test]
    fn escape_ending_a_sequence_is_consumed_and_otherwise_ordinary() {
        let mut k = keys(Some(&table()));
        let escape = |k: &mut KeyText| {
            let sym = k.xkb.key_get_one_sym(xkb::Keycode::new(ESC + 8));
            k.press(HERE, ESC, sym, false, Zeroizing::new(String::new()))
                .is_some()
        };
        assert_eq!(press(&mut k, DEAD), "");
        assert!(!escape(&mut k), "the press is spent");
        assert!(k.take_consumed_release(ESC), "and its release");
        assert!(!k.take_consumed_release(ESC), "once");
        assert_eq!(press(&mut k, E), "e", "the sequence is over");
        assert!(escape(&mut k), "nothing pending");
        assert!(!k.take_consumed_release(ESC));
        // Nor is a finished or cancelled sequence pending.
        press(&mut k, DEAD);
        assert_eq!(press(&mut k, E), "é");
        assert!(escape(&mut k), "after a composed key");
        press(&mut k, DEAD);
        assert_eq!(press(&mut k, A), "");
        assert!(escape(&mut k), "after a cancel");
        // With no table no key is spent.
        let mut plain = keys(None);
        assert!(escape(&mut plain));
    }

    #[test]
    fn a_sequence_ends_when_the_keyboard_leaves_or_the_surface_changes() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, DEAD), "");
        k.reset();
        assert_eq!(press(&mut k, E), "e");
        assert_eq!(press_on(&mut k, HERE, DEAD, ""), "");
        assert_eq!(press_on(&mut k, THERE, E, ""), "e");
        // And the same surface keeps it.
        assert_eq!(press_on(&mut k, THERE, DEAD, ""), "");
        assert_eq!(press_on(&mut k, THERE, E, ""), "é");
    }

    /// No table: what SCTK gave, untouched, for every key.
    #[test]
    fn with_no_table_a_key_types_what_sctk_says() {
        let mut k = keys(None);
        assert_eq!(press(&mut k, DEAD), "from sctk");
        assert_eq!(press(&mut k, E), "from sctk");
        assert_eq!(press_on(&mut k, HERE, E, ""), "");
        // A table found later composes from then on.
        k.set_table(&table());
        assert_eq!(press(&mut k, DEAD), "");
        assert_eq!(press(&mut k, E), "é");
    }

    /// Ctrl, Alt or Super held: no key of a sequence, and it ends one.
    #[test]
    fn a_chord_keeps_its_own_text_and_ends_a_sequence() {
        let mut k = keys(Some(&table()));
        assert_eq!(press(&mut k, DEAD), "");
        let sym = k.xkb.key_get_one_sym(xkb::Keycode::new(E + 8));
        let chord = k
            .press(HERE, E, sym, true, Zeroizing::new(String::new()))
            .expect("a chord is delivered");
        assert_eq!(*chord, "e");
        assert!(!k.pending());
        assert_eq!(press(&mut k, E), "e");
    }

    #[test]
    fn the_locale_is_the_first_of_lc_all_lc_ctype_and_lang() {
        let with = |set: &'static [(&'static str, &'static str)]| {
            locale_from(|name| {
                set.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| OsString::from(*v))
            })
        };
        assert_eq!(with(&[]), "C");
        assert_eq!(with(&[("LANG", "de_DE.UTF-8")]), "de_DE.UTF-8");
        assert_eq!(
            with(&[("LANG", "de_DE.UTF-8"), ("LC_CTYPE", "fr_FR.UTF-8")]),
            "fr_FR.UTF-8"
        );
        assert_eq!(
            with(&[("LC_ALL", ""), ("LC_CTYPE", "fr_FR.UTF-8"), ("LANG", "x")]),
            "fr_FR.UTF-8"
        );
        assert_eq!(
            with(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "x")]),
            "en_US.UTF-8"
        );
    }
}
