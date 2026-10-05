//! The hello bar of `docs/design.md` as a retained scene:
//!
//! ```text
//! bar Top {
//!   edge: top; height: 32
//!   split {
//!     start  { text windows.focused?.title ?? "" }
//!     center { text clock.format("%H:%M") }
//!     end    { text pct(battery.percent) }
//!   }
//! }
//! ```
//!
//! M0 has no compiler, no flex layout and no services yet, so this module
//! is what the compiler will emit for that file, written by hand: the
//! `split` sections span the bar and align their text to the start, centre
//! and end (M0 layout is absolute placement; taffy arrives in M2), and the
//! start and end texts are placeholders until the window and battery
//! services land in M3.

use strand_scene::{Color, Font, Length, NodeId, NodeKind, Prop, PropValue, SceneDiff};

pub const BAR: NodeId = NodeId::new(0, 0);
pub const SPLIT: NodeId = NodeId::new(1, 0);
pub const START: NodeId = NodeId::new(2, 0);
pub const CENTER: NodeId = NodeId::new(3, 0);
pub const END: NodeId = NodeId::new(4, 0);
pub const START_TEXT: NodeId = NodeId::new(5, 0);
pub const CLOCK: NodeId = NodeId::new(6, 0);
pub const END_TEXT: NodeId = NodeId::new(7, 0);

/// Bar thickness in logical pixels (`height: 32`).
pub const HEIGHT: f32 = 32.0;
/// Font size of `$font.ui` (`"Inter" 13px 500`).
pub const FONT_SIZE: f32 = 13.0;
/// Horizontal padding of the sections (`$space.3`).
pub const PAD: f32 = 12.0;

/// Placeholder for `windows.focused?.title ?? ""` (windows service: M3).
pub const START_PLACEHOLDER: &str = "Strand";
/// Placeholder for `pct(battery.percent)` (battery service: M3).
pub const END_PLACEHOLDER: &str = "M0 demo";

fn full() -> PropValue {
    PropValue::Length(Length::Percent(100.0))
}

fn color(hex: &str) -> PropValue {
    // The literals below are valid; a bad one would read as unset.
    Color::from_hex(hex).map_or(PropValue::Unset, PropValue::Color)
}

/// The boot diff: the whole tree. The clock's text is not part of it: the
/// logic thread's clock effect sets it ([`clock`]) in the same tick.
pub fn bar() -> SceneDiff {
    let mut d = SceneDiff::new();
    d.create(BAR, NodeKind::Bar, None, 0);
    d.set(BAR, Prop::Name, PropValue::Text("Top".into()));
    d.set(BAR, Prop::Edge, PropValue::Keyword("top".into()));
    d.set(BAR, Prop::Height, PropValue::Number(HEIGHT));
    // Theme defaults until tokens and palettes are wired (M2).
    d.set(BAR, Prop::Bg, color("#1e1e2e"));
    d.set(BAR, Prop::Color, color("#cdd6f4"));
    d.set(
        BAR,
        Prop::Font,
        PropValue::Font(Font {
            family: "Inter, sans-serif".into(),
            size: FONT_SIZE,
            weight: 500,
        }),
    );

    d.create(SPLIT, NodeKind::Split, Some(BAR), 0);
    d.set(SPLIT, Prop::Width, full());
    d.set(SPLIT, Prop::Height, full());
    let sections = [
        (START, START_TEXT, PAD, "start", Some(START_PLACEHOLDER)),
        (CENTER, CLOCK, 0.0, "center", None),
        (END, END_TEXT, -PAD, "end", Some(END_PLACEHOLDER)),
    ];
    for (i, (section, text, x, align, value)) in sections.into_iter().enumerate() {
        let kind = match align {
            "start" => NodeKind::Start,
            "center" => NodeKind::Center,
            _ => NodeKind::End,
        };
        d.create(section, kind, Some(SPLIT), i as u32);
        d.set(section, Prop::X, PropValue::Number(x));
        d.set(section, Prop::Width, full());
        d.set(section, Prop::Height, full());
        d.create(text, NodeKind::Text, Some(section), 0);
        if let Some(value) = value {
            d.set(text, Prop::Text, PropValue::Text(value.into()));
        }
        d.set(text, Prop::Width, full());
        d.set(text, Prop::Align, PropValue::Keyword(align.into()));
        // Vertically centred for the font's line height (about 1.2 em).
        let y = ((HEIGHT - FONT_SIZE * 1.2) / 2.0).round();
        d.set(text, Prop::Y, PropValue::Number(y));
    }
    d
}

/// The diff for a clock change: one text prop.
pub fn clock(text: &str) -> SceneDiff {
    let mut d = SceneDiff::new();
    d.set(CLOCK, Prop::Text, PropValue::Text(text.into()));
    d
}
