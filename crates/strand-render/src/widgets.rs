//! Render-side widget state: what the input router knows that changes
//! how a widget draws before logic answers (design.md: input stays on the
//! render thread, so a slider follows the pointer and a caret moves on
//! the frame the key arrives).
//!
//! The [`crate::Router`] writes it through [`crate::InputScene`]; flatten
//! reads it. Logic still hears every change as flags and two-way writes.

use std::collections::{HashMap, HashSet};

use strand_scene::{Insets, NodeId, PropValue};

/// An `input`'s caret and selection, as byte offsets into its text: the
/// selection runs between `anchor` and `pos` (empty when they are equal).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Caret {
    pub pos: usize,
    pub anchor: usize,
}

impl Caret {
    pub fn at(pos: usize) -> Self {
        Self { pos, anchor: pos }
    }

    /// The selected byte range (empty when nothing is selected).
    pub fn selection(&self) -> std::ops::Range<usize> {
        self.pos.min(self.anchor)..self.pos.max(self.anchor)
    }

    /// Both ends clamped to `text`'s length and moved back onto
    /// character boundaries.
    pub fn clamped(self, text: &str) -> Self {
        let fix = |mut i: usize| {
            i = i.min(text.len());
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        Self {
            pos: fix(self.pos),
            anchor: fix(self.anchor),
        }
    }
}

/// Input state widgets draw with.
#[derive(Clone, Debug, Default)]
pub struct Widgets {
    pub hovered: HashSet<NodeId>,
    pub pressed: HashSet<NodeId>,
    pub focused: HashSet<NodeId>,
    /// Each `input`'s caret (inputs never focused have none: their caret
    /// would sit at the end).
    pub carets: HashMap<NodeId, Caret>,
    /// A slider's value while it is dragged: drawn instead of `value`
    /// until the drag ends (the writes are still on their way through
    /// logic).
    pub drags: HashMap<NodeId, f32>,
}

impl Widgets {
    /// Forgets state of nodes `live` says are gone.
    pub fn retain(&mut self, live: impl Fn(NodeId) -> bool) {
        self.hovered.retain(|n| live(*n));
        self.pressed.retain(|n| live(*n));
        self.focused.retain(|n| live(*n));
        self.carets.retain(|n, _| live(*n));
        self.drags.retain(|n, _| live(*n));
    }
}

/// A button's padding when it sets none, logical pixels.
pub const BUTTON_PAD: Insets = Insets {
    top: 4.0,
    right: 10.0,
    bottom: 4.0,
    left: 10.0,
};

/// Padding of each `segmented` option around its label, logical pixels.
pub const SEGMENT_PAD: f32 = 10.0;

/// Width of a slider that sets none, logical pixels.
pub const SLIDER_WIDTH: f32 = 120.0;

/// A slider's track thickness, logical pixels.
pub const SLIDER_TRACK: f32 = 4.0;

/// A slider knob's diameter (one more pixel on each side while hovered or
/// dragged), logical pixels.
pub const SLIDER_KNOB: f32 = 14.0;

/// An `input` caret's width, logical pixels.
pub const CARET_WIDTH: f32 = 1.5;

/// The options of a `segmented` (`options: Look` arrives as a list of
/// the enum's variants as keywords; a list of texts or numbers works
/// too).
pub fn options(v: Option<&PropValue>) -> Vec<PropValue> {
    match v {
        Some(PropValue::List(items)) => items
            .iter()
            .filter(|i| !matches!(i, PropValue::List(_) | PropValue::Unset))
            .take(64)
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// The label an option shows: its name as written, `_` as spaces.
pub fn option_label(v: &PropValue) -> String {
    match v {
        PropValue::Keyword(k) => k.replace('_', " "),
        PropValue::Text(t) => t.clone(),
        PropValue::Number(n) => format!("{n}"),
        PropValue::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

/// True if `a` is the option `b` (keywords and texts compare by name, so
/// a value written back as a keyword matches a text option).
pub fn same_option(a: &PropValue, b: &PropValue) -> bool {
    match (a, b) {
        (
            PropValue::Keyword(x) | PropValue::Text(x),
            PropValue::Keyword(y) | PropValue::Text(y),
        ) => x == y,
        (PropValue::Number(x), PropValue::Number(y)) => x == y,
        _ => a == b,
    }
}

/// The text an `input` shows for `text`: itself, or one bullet per
/// character for `type: password`. Returns the shown text and a map from
/// a byte offset in `text` to one in the shown text.
pub fn shown_text(text: &str, password: bool) -> (String, Box<dyn Fn(usize) -> usize>) {
    if !password {
        return (text.to_string(), Box::new(|i| i));
    }
    const BULLET: char = '•';
    let n = text.chars().count();
    let shown: String = std::iter::repeat_n(BULLET, n).collect();
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let w = BULLET.len_utf8();
    (
        shown,
        Box::new(move |i| starts.partition_point(|s| *s < i) * w),
    )
}

/// What a key does to an `input`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Nothing (a key an input does not take).
    None,
    /// The caret or selection moved; the text is unchanged.
    Moved(Caret),
    /// The text changed, and the caret with it.
    Changed(String, Caret),
}

fn prev_char(text: &str, i: usize) -> usize {
    text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
}

fn next_char(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}

/// The start of the word before `i` (skipping spaces first), as
/// Ctrl+Left and Ctrl+BackSpace go.
fn prev_word(text: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 && text[..j].ends_with(char::is_whitespace) {
        j = prev_char(text, j);
    }
    while j > 0 && !text[..j].ends_with(char::is_whitespace) {
        j = prev_char(text, j);
    }
    j
}

/// The end of the word after `i`.
fn next_word(text: &str, i: usize) -> usize {
    let mut j = i;
    while j < text.len() && text[j..].starts_with(char::is_whitespace) {
        j = next_char(text, j);
    }
    while j < text.len() && !text[j..].starts_with(char::is_whitespace) {
        j = next_char(text, j);
    }
    j
}

/// What key `name` (typing `typed`) does to an input holding `text` with
/// `caret`: typing replaces the selection; BackSpace and Delete remove it
/// or the character (Ctrl: the word) before or after the caret; Left,
/// Right, Home and End move the caret (Shift extends the selection, Ctrl
/// moves by words); Ctrl+A selects everything. Keys with Ctrl, Alt or
/// Super type nothing.
pub fn edit(text: &str, caret: Caret, name: &str, typed: &str, m: strand_scene::Modifiers) -> Edit {
    let c = caret.clamped(text);
    let sel = c.selection();
    let to = |pos: usize| {
        if m.shift {
            Caret {
                pos,
                anchor: c.anchor,
            }
        } else {
            Caret::at(pos)
        }
    };
    let replace = |range: std::ops::Range<usize>, with: &str| {
        let mut t = String::with_capacity(text.len() + with.len());
        t.push_str(&text[..range.start]);
        t.push_str(with);
        t.push_str(&text[range.end..]);
        Edit::Changed(t, Caret::at(range.start + with.len()))
    };
    match name {
        "Left" | "KP_Left" => {
            let pos = if !m.shift && !sel.is_empty() {
                sel.start
            } else if m.ctrl {
                prev_word(text, c.pos)
            } else {
                prev_char(text, c.pos)
            };
            Edit::Moved(to(pos))
        }
        "Right" | "KP_Right" => {
            let pos = if !m.shift && !sel.is_empty() {
                sel.end
            } else if m.ctrl {
                next_word(text, c.pos)
            } else {
                next_char(text, c.pos)
            };
            Edit::Moved(to(pos))
        }
        "Home" | "KP_Home" => Edit::Moved(to(0)),
        "End" | "KP_End" => Edit::Moved(to(text.len())),
        "a" | "A" if m.ctrl && !m.alt => Edit::Moved(Caret {
            pos: text.len(),
            anchor: 0,
        }),
        "BackSpace" => {
            if !sel.is_empty() {
                replace(sel, "")
            } else if c.pos == 0 {
                Edit::None
            } else if m.ctrl {
                replace(prev_word(text, c.pos)..c.pos, "")
            } else {
                replace(prev_char(text, c.pos)..c.pos, "")
            }
        }
        "Delete" | "KP_Delete" => {
            if !sel.is_empty() {
                replace(sel, "")
            } else if c.pos >= text.len() {
                Edit::None
            } else if m.ctrl {
                replace(c.pos..next_word(text, c.pos), "")
            } else {
                replace(c.pos..next_char(text, c.pos), "")
            }
        }
        _ if !typed.is_empty()
            && !m.ctrl
            && !m.alt
            && !m.logo
            && !typed.chars().any(char::is_control) =>
        {
            replace(sel, typed)
        }
        _ => Edit::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carets_clamp_to_character_boundaries() {
        let c = Caret { pos: 2, anchor: 9 }.clamped("héllo");
        assert_eq!(c, Caret { pos: 1, anchor: 6 });
        assert_eq!(c.selection(), 1..6);
    }

    #[test]
    fn passwords_show_bullets() {
        let (s, map) = shown_text("hé!", true);
        assert_eq!(s, "•••");
        assert_eq!(map(0), 0);
        assert_eq!(map(1), 3);
        assert_eq!(map(3), 6);
        assert_eq!(map(4), 9);
    }

    #[test]
    fn editing_at_the_caret() {
        use strand_scene::Modifiers;
        let none = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..none
        };
        let ctrl = Modifiers { ctrl: true, ..none };
        // Typing inserts at the caret.
        assert_eq!(
            edit("fir", Caret::at(1), "x", "x", none),
            Edit::Changed("fxir".into(), Caret::at(2))
        );
        // Typing replaces the selection.
        assert_eq!(
            edit("firefox", Caret { pos: 4, anchor: 0 }, "w", "w", none),
            Edit::Changed("wfox".into(), Caret::at(1))
        );
        // BackSpace and Delete, by character and by word.
        assert_eq!(
            edit("héllo", Caret::at(3), "BackSpace", "", none),
            Edit::Changed("hllo".into(), Caret::at(1))
        );
        assert_eq!(
            edit("ab cd", Caret::at(5), "BackSpace", "", ctrl),
            Edit::Changed("ab ".into(), Caret::at(3))
        );
        assert_eq!(
            edit("ab", Caret::at(0), "Delete", "", none),
            Edit::Changed("b".into(), Caret::at(0))
        );
        assert_eq!(edit("ab", Caret::at(0), "BackSpace", "", none), Edit::None);
        // Moving, extending, word jumps, select all.
        assert_eq!(
            edit("héllo", Caret::at(3), "Left", "", none),
            Edit::Moved(Caret::at(1))
        );
        assert_eq!(
            edit(
                "ab cd",
                Caret::at(5),
                "Left",
                "",
                Modifiers {
                    shift: true,
                    ctrl: true,
                    ..none
                }
            ),
            Edit::Moved(Caret { pos: 3, anchor: 5 })
        );
        assert_eq!(
            edit("abc", Caret::at(1), "End", "", shift),
            Edit::Moved(Caret { pos: 3, anchor: 1 })
        );
        assert_eq!(
            edit("abc", Caret { pos: 3, anchor: 1 }, "Left", "", none),
            Edit::Moved(Caret::at(1)),
            "Left collapses a selection to its start"
        );
        assert_eq!(
            edit("abc", Caret::at(1), "a", "a", ctrl),
            Edit::Moved(Caret { pos: 3, anchor: 0 })
        );
        // Control keys type nothing.
        assert_eq!(edit("abc", Caret::at(1), "Tab", "\t", none), Edit::None);
    }

    #[test]
    fn option_labels() {
        assert_eq!(
            option_label(&PropValue::Keyword("tonal_spot".into())),
            "tonal spot"
        );
        assert!(same_option(
            &PropValue::Keyword("dark".into()),
            &PropValue::Text("dark".into())
        ));
    }
}
