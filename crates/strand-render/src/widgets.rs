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

/// A slider knob's radius: [`SLIDER_KNOB`] across, 2 px more while the
/// slider is hovered or pressed (always so during a drag).
pub fn slider_knob_radius(active: bool) -> f64 {
    (SLIDER_KNOB as f64 + if active { 2.0 } else { 0.0 }) / 2.0
}

/// Where a slider's knob centre runs from value 0 to value 1 across its
/// box `x0..x1`, in units of `s` per logical pixel: drawing and dragging
/// share it, so the knob stays under the pointer.
pub fn slider_span(x0: f64, x1: f64, active: bool, s: f64) -> (f64, f64) {
    let k = slider_knob_radius(active) * s;
    (x0 + k, (x1 - k).max(x0 + k))
}

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
/// character for `type: password`. Returns the shown text when it is not
/// `text` itself (a password's bullets), and a map from a byte offset in
/// `text` to one in the shown text.
pub fn shown_text(text: &str, password: bool) -> (Option<String>, Box<dyn Fn(usize) -> usize>) {
    if !password {
        return (None, Box::new(|i| i));
    }
    const BULLET: char = '•';
    let n = text.chars().count();
    let shown: String = std::iter::repeat_n(BULLET, n).collect();
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let w = BULLET.len_utf8();
    (
        Some(shown),
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
    /// (M6) Copy the selected text to the compositor's clipboard (Ctrl+C,
    /// Ctrl+Insert).
    Copy(String),
    /// (M6) Cut: copy `copied`, and the text and caret change as
    /// [`Edit::Changed`] does (Ctrl+X, Shift+Delete).
    Cut {
        copied: String,
        text: String,
        caret: Caret,
    },
    /// (M6) Paste the clipboard's text at the caret (Ctrl+V,
    /// Shift+Insert): the host reads it and [`pasted`] inserts it.
    Paste,
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
/// moves by words); Ctrl+A selects everything. (M6) Ctrl+C and
/// Ctrl+Insert copy the selection, Ctrl+X and Shift+Delete cut it, Ctrl+V
/// and Shift+Insert paste (Ctrl+Shift+C/X/V too); a `password` input
/// copies and cuts nothing. Keys with Ctrl, Alt or Super type nothing.
pub fn edit(
    text: &str,
    caret: Caret,
    name: &str,
    typed: &str,
    m: strand_scene::Modifiers,
    password: bool,
) -> Edit {
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
    let selected = (!sel.is_empty() && !password).then(|| text[sel.clone()].to_string());
    match name {
        "c" | "C" if m.ctrl && !m.alt => selected.map_or(Edit::None, Edit::Copy),
        "Insert" | "KP_Insert" if m.ctrl && !m.shift => selected.map_or(Edit::None, Edit::Copy),
        "x" | "X" if m.ctrl && !m.alt => cut(text, sel, selected),
        "Delete" | "KP_Delete" if m.shift && !m.ctrl => cut(text, sel, selected),
        "v" | "V" if m.ctrl && !m.alt => Edit::Paste,
        "Insert" | "KP_Insert" if m.shift && !m.ctrl => Edit::Paste,
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

/// A cut of `sel` from `text`: nothing without a selection (or of a
/// password, `selected` being `None`).
fn cut(text: &str, sel: std::ops::Range<usize>, selected: Option<String>) -> Edit {
    let Some(copied) = selected else {
        return Edit::None;
    };
    let mut t = String::with_capacity(text.len());
    t.push_str(&text[..sel.start]);
    t.push_str(&text[sel.end..]);
    Edit::Cut {
        copied,
        text: t,
        caret: Caret::at(sel.start),
    }
}

/// (M6) The clipboard's text `pasted` into an input holding `text` with
/// `caret`: it replaces the selection and the caret goes after it. An
/// `input` is one line, so each line break (`\r\n`, `\n`, `\r`) and tab
/// becomes one space and other control characters are dropped; nothing
/// left is [`Edit::None`].
pub fn pasted(text: &str, caret: Caret, pasted: &str) -> Edit {
    let mut line = String::with_capacity(pasted.len());
    let mut chars = pasted.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line.push(' ');
            }
            '\n' | '\t' => line.push(' '),
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    if line.is_empty() {
        return Edit::None;
    }
    let c = caret.clamped(text);
    let sel = c.selection();
    let mut t = String::with_capacity(text.len() + line.len());
    t.push_str(&text[..sel.start]);
    t.push_str(&line);
    t.push_str(&text[sel.end..]);
    Edit::Changed(t, Caret::at(sel.start + line.len()))
}

/// (M4) An `arc`'s line width when it gives none, logical pixels.
pub const ARC_WIDTH: f32 = 4.0;

/// (M4) An `arc`'s sweep when it gives none, degrees.
pub const ARC_SWEEP: f32 = 270.0;

/// (M4) A wavy `meter`'s wavelength, logical pixels (Material 3's wavy
/// linear progress indicator).
pub const METER_WAVELENGTH: f32 = 40.0;

/// (M4) An `arc` gauge's centre lines in `frame` (physical pixels) for a
/// line `width` pixels wide: the track (the whole sweep, its gap centred
/// at the bottom; a full ring starts at 12 o'clock) and the value (the
/// fraction `value` of it from its start, clockwise). `None` when the box
/// cannot hold the line.
pub(crate) fn arc_lines(
    frame: vello_cpu::kurbo::Rect,
    value: f64,
    sweep: f64,
    width: f64,
) -> Option<(vello_cpu::kurbo::BezPath, Option<vello_cpu::kurbo::BezPath>)> {
    let r = (frame.width().min(frame.height()) - width) / 2.0;
    if !(r > 0.0 && sweep > 0.0) {
        return None;
    }
    let sweep = sweep.min(360.0);
    let start = if sweep >= 360.0 {
        -90.0
    } else {
        90.0 + (360.0 - sweep) / 2.0
    };
    let c = frame.center();
    let track = crate::shapes::stroke::arc(c, r, start, sweep);
    let v = value.clamp(0.0, 1.0);
    let fill = (v > 0.0).then(|| crate::shapes::stroke::arc(c, r, start, sweep * v));
    Some((track, fill))
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
        assert_eq!(s.as_deref(), Some("•••"));
        assert!(shown_text("plain", false).0.is_none());
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
            edit("fir", Caret::at(1), "x", "x", none, false),
            Edit::Changed("fxir".into(), Caret::at(2))
        );
        // Typing replaces the selection.
        assert_eq!(
            edit(
                "firefox",
                Caret { pos: 4, anchor: 0 },
                "w",
                "w",
                none,
                false
            ),
            Edit::Changed("wfox".into(), Caret::at(1))
        );
        // BackSpace and Delete, by character and by word.
        assert_eq!(
            edit("héllo", Caret::at(3), "BackSpace", "", none, false),
            Edit::Changed("hllo".into(), Caret::at(1))
        );
        assert_eq!(
            edit("ab cd", Caret::at(5), "BackSpace", "", ctrl, false),
            Edit::Changed("ab ".into(), Caret::at(3))
        );
        assert_eq!(
            edit("ab", Caret::at(0), "Delete", "", none, false),
            Edit::Changed("b".into(), Caret::at(0))
        );
        assert_eq!(
            edit("ab", Caret::at(0), "BackSpace", "", none, false),
            Edit::None
        );
        // Moving, extending, word jumps, select all.
        assert_eq!(
            edit("héllo", Caret::at(3), "Left", "", none, false),
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
                },
                false
            ),
            Edit::Moved(Caret { pos: 3, anchor: 5 })
        );
        assert_eq!(
            edit("abc", Caret::at(1), "End", "", shift, false),
            Edit::Moved(Caret { pos: 3, anchor: 1 })
        );
        assert_eq!(
            edit("abc", Caret { pos: 3, anchor: 1 }, "Left", "", none, false),
            Edit::Moved(Caret::at(1)),
            "Left collapses a selection to its start"
        );
        assert_eq!(
            edit("abc", Caret::at(1), "a", "a", ctrl, false),
            Edit::Moved(Caret { pos: 3, anchor: 0 })
        );
        // Control keys type nothing.
        assert_eq!(
            edit("abc", Caret::at(1), "Tab", "\t", none, false),
            Edit::None
        );
    }

    #[test]
    fn copy_cut_and_paste_keys() {
        use strand_scene::Modifiers;
        let none = Modifiers::default();
        let ctrl = Modifiers { ctrl: true, ..none };
        let shift = Modifiers {
            shift: true,
            ..none
        };
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..none
        };
        let sel = Caret { pos: 4, anchor: 1 };
        let key = |name: &str, typed: &str, m, c| edit("abcdef", c, name, typed, m, false);
        // Copy: Ctrl+C, Ctrl+Insert, Ctrl+Shift+C; nothing without a
        // selection.
        assert_eq!(key("c", "c", ctrl, sel), Edit::Copy("bcd".into()));
        assert_eq!(key("Insert", "", ctrl, sel), Edit::Copy("bcd".into()));
        assert_eq!(key("C", "C", ctrl_shift, sel), Edit::Copy("bcd".into()));
        assert_eq!(key("c", "c", ctrl, Caret::at(2)), Edit::None);
        // Cut: Ctrl+X and Shift+Delete.
        let cut = Edit::Cut {
            copied: "bcd".into(),
            text: "aef".into(),
            caret: Caret::at(1),
        };
        assert_eq!(key("x", "x", ctrl, sel), cut);
        assert_eq!(key("Delete", "", shift, sel), cut);
        assert_eq!(
            key("Delete", "", shift, Caret::at(2)),
            Edit::None,
            "Shift+Delete cuts nothing without a selection"
        );
        // Plain Delete is unchanged.
        assert_eq!(
            key("Delete", "", none, sel),
            Edit::Changed("aef".into(), Caret::at(1))
        );
        assert_eq!(
            key("Delete", "", none, Caret::at(0)),
            Edit::Changed("bcdef".into(), Caret::at(0))
        );
        // Paste: Ctrl+V, Shift+Insert, Ctrl+Shift+V.
        assert_eq!(key("v", "v", ctrl, sel), Edit::Paste);
        assert_eq!(key("Insert", "", shift, sel), Edit::Paste);
        assert_eq!(key("V", "V", ctrl_shift, Caret::at(0)), Edit::Paste);
        // A plain `c` still types.
        assert_eq!(
            key("c", "c", none, Caret::at(0)),
            Edit::Changed("cabcdef".into(), Caret::at(1))
        );
    }

    #[test]
    fn a_password_copies_and_cuts_nothing() {
        use strand_scene::Modifiers;
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let all = Caret { pos: 6, anchor: 0 };
        let key = |name: &str, m| edit("secret", all, name, "", m, true);
        assert_eq!(key("c", ctrl), Edit::None);
        assert_eq!(key("Insert", ctrl), Edit::None);
        assert_eq!(key("x", ctrl), Edit::None);
        assert_eq!(key("Delete", shift), Edit::None);
        assert_eq!(key("v", ctrl), Edit::Paste, "a password takes a paste");
    }

    #[test]
    fn a_paste_is_one_line() {
        assert_eq!(
            pasted("ab", Caret::at(1), "x\r\ny\tz\u{7}"),
            Edit::Changed("ax y zb".into(), Caret::at(6))
        );
        assert_eq!(
            pasted("hello", Caret { pos: 1, anchor: 4 }, "\n\r"),
            Edit::Changed("h  o".into(), Caret::at(3)),
            "a paste replaces the selection; `\\n` and `\\r` apart are two breaks"
        );
        assert_eq!(
            pasted("héllo", Caret::at(9), "!"),
            Edit::Changed("héllo!".into(), Caret::at(7)),
            "the caret is clamped first"
        );
        assert_eq!(pasted("ab", Caret::at(1), ""), Edit::None);
        assert_eq!(pasted("ab", Caret::at(1), "\u{1b}"), Edit::None);
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
