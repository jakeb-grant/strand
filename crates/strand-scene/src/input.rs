//! Input events: what `strand-surface` reads from Wayland and hands to
//! render (hit testing on the rounded shape, `hover`/`pressed`) and, as
//! node events, to logic ("Render → logic is `InputEvent`s").
//!
//! Positions are surface-local logical pixels (what `wl_pointer` reports),
//! so they are independent of the buffer scale. Wayland serials stay in
//! `strand-surface`. Keyboard events arrive on `keyboard: on_demand |
//! exclusive` surfaces.

use std::path::PathBuf;

use crate::protocol::named_enum;
use crate::{LogicalPoint, NodeId, SurfaceId};

/// Linux evdev button codes (`linux/input-event-codes.h`) as `wl_pointer`
/// reports them.
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
}

/// Whether a button went down or up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// What produced a scroll.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
    Continuous,
    WheelTilt,
}

/// Scroll along one axis within one pointer frame.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct AxisDelta {
    /// Logical pixels.
    pub pixels: f64,
    /// Wheel detents in 1/120 steps (`axis_value120`; legacy discrete
    /// steps are converted ×120). 0 for touchpads.
    pub value120: i32,
    /// The scroll on this axis stopped (kinetic scrolling may start).
    pub stop: bool,
}

impl AxisDelta {
    pub fn is_zero(&self) -> bool {
        self.pixels == 0.0 && self.value120 == 0 && !self.stop
    }
}

/// Modifier keys held during a key event.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

/// One key press, release or repeat, as xkbcommon resolved it.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyInput {
    /// The keysym's name (`Escape`, `Return`, `Down`, `BackSpace`, `a`):
    /// what `on key(k)` sees as `k.name`.
    pub name: String,
    /// The text the key types (empty for keys that type none).
    pub text: String,
    pub state: ButtonState,
    /// A key-repeat of a held key (a press).
    pub repeat: bool,
    pub modifiers: Modifiers,
    /// Milliseconds, from the compositor's clock.
    pub time: u32,
}

named_enum! {
    /// (M4) What another program dropped: builtin.schema's `enum
    /// DropKind`.
    pub enum DropKind {
        Files = "files",
        App = "app",
        Text = "text",
    }
}

/// (M4) What a drop carries (design.md, "Drag and drop").
#[derive(Clone, Debug, PartialEq)]
pub enum DropPayload {
    /// A `drag:` source in Strand: logic maps the node back to its
    /// value.
    Node(NodeId),
    /// Another program's drop. Only the fields `kind` names are filled;
    /// logic resolves `app_id` to an `App` through the `apps` service
    /// (null when no installed app has it).
    External {
        kind: DropKind,
        files: Vec<PathBuf>,
        text: String,
        app_id: Option<String>,
    },
}

/// (M4) The dragged value's type name in a `drag:` source's
/// [`Prop::Drag`](crate::Prop::Drag): a `Keyword`, or the first item of
/// the `List` that also carries its [`drag_export`].
pub fn drag_type(value: &crate::PropValue) -> Option<&str> {
    use crate::PropValue as V;
    match value {
        V::Keyword(k) => Some(k),
        V::List(items) => match items.first()? {
            V::Keyword(k) => Some(k),
            _ => None,
        },
        _ => None,
    }
}

/// (M4) What a `drag:` source gives other programs when the compositor
/// carries it out (`None`: nothing, the drag is Strand's alone). The
/// compiler sets [`Prop::Drag`](crate::Prop::Drag) to `[type, kind,
/// items…]` for a value that has a form outside Strand: `text`
/// (`[type, text, Text]`), a `Drop` (its kind; `files`: a `Text` per
/// path, `text`: one `Text`, `app`: the app's desktop id), an `App`
/// (`[type, app, id]`).
pub fn drag_export(value: &crate::PropValue) -> Option<DropPayload> {
    use crate::PropValue as V;
    let V::List(items) = value else {
        return None;
    };
    let kind = match items.get(1)? {
        V::Keyword(k) => DropKind::from_name(k)?,
        _ => return None,
    };
    let texts: Vec<&str> = items[2..]
        .iter()
        .filter_map(|v| match v {
            V::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let first = texts.first().map(|t| (*t).to_string());
    Some(match kind {
        DropKind::Files => DropPayload::External {
            kind,
            files: texts.iter().map(PathBuf::from).collect(),
            text: String::new(),
            app_id: None,
        },
        DropKind::Text => DropPayload::External {
            kind,
            files: Vec::new(),
            text: first?,
            app_id: None,
        },
        DropKind::App => DropPayload::External {
            kind,
            files: Vec::new(),
            text: String::new(),
            app_id: Some(first?),
        },
    })
}

/// An input event on one of our surfaces.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// The pointer entered `surface` at `position`.
    PointerEnter {
        surface: SurfaceId,
        position: LogicalPoint,
    },
    /// The pointer left `surface`.
    PointerLeave { surface: SurfaceId },
    PointerMotion {
        surface: SurfaceId,
        position: LogicalPoint,
        /// Milliseconds, from the compositor's clock.
        time: u32,
    },
    PointerButton {
        surface: SurfaceId,
        position: LogicalPoint,
        /// evdev code, see [`button`].
        button: u32,
        state: ButtonState,
        time: u32,
    },
    /// Scrolling; `vertical.pixels > 0` scrolls down.
    PointerAxis {
        surface: SurfaceId,
        position: LogicalPoint,
        horizontal: AxisDelta,
        vertical: AxisDelta,
        source: Option<AxisSource>,
        time: u32,
    },
    /// `surface` got keyboard focus (`keyboard: on_demand | exclusive`).
    KeyboardEnter { surface: SurfaceId },
    /// `surface` lost keyboard focus.
    KeyboardLeave { surface: SurfaceId },
    /// A key on the focused surface.
    Key { surface: SurfaceId, key: KeyInput },
    /// A button was pressed outside `surface`, on the transparent
    /// click-away catcher mapped under it (an open `keyboard: exclusive`
    /// surface whose `open` is two-way): it closes.
    ClickAway { surface: SurfaceId },
    /// (M4) A drag entered `surface` through `wl_data_device` (another
    /// program's, or one between Strand surfaces), offering `kinds`.
    DragEnter {
        surface: SurfaceId,
        at: LogicalPoint,
        kinds: Vec<DropKind>,
    },
    /// (M4) The drag moved over `surface`.
    DragMotion {
        surface: SurfaceId,
        at: LogicalPoint,
    },
    /// (M4) The drag left `surface` without dropping.
    DragLeave { surface: SurfaceId },
    /// (M4) The drag dropped on `surface`.
    DragDrop {
        surface: SurfaceId,
        at: LogicalPoint,
        payload: DropPayload,
    },
}

impl InputEvent {
    /// The surface the event happened on.
    pub fn surface(&self) -> SurfaceId {
        match self {
            Self::PointerEnter { surface, .. }
            | Self::PointerLeave { surface }
            | Self::PointerMotion { surface, .. }
            | Self::PointerButton { surface, .. }
            | Self::PointerAxis { surface, .. }
            | Self::KeyboardEnter { surface }
            | Self::KeyboardLeave { surface }
            | Self::Key { surface, .. }
            | Self::ClickAway { surface }
            | Self::DragEnter { surface, .. }
            | Self::DragMotion { surface, .. }
            | Self::DragLeave { surface }
            | Self::DragDrop { surface, .. } => *surface,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_kinds_round_trip() {
        for k in DropKind::ALL {
            assert_eq!(DropKind::from_name(k.name()), Some(*k));
        }
        assert_eq!(DropKind::from_name("uri"), None);
    }

    /// A `drag:` source's prop: its type name, alone or before what it
    /// gives other programs.
    #[test]
    fn a_drag_prop_names_its_type_and_what_it_exports() {
        use crate::PropValue as V;
        let kw = |k: &str| V::Keyword(k.into());
        let t = |k: &str| V::Text(k.into());
        assert_eq!(drag_type(&kw("Pin")), Some("Pin"));
        assert_eq!(drag_export(&kw("Pin")), None);
        let text = V::List(vec![kw("text"), kw("text"), t("hello")]);
        assert_eq!(drag_type(&text), Some("text"));
        assert_eq!(
            drag_export(&text),
            Some(DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "hello".into(),
                app_id: None,
            })
        );
        let files = V::List(vec![kw("Drop"), kw("files"), t("/a b"), t("/c")]);
        assert_eq!(
            drag_export(&files),
            Some(DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/a b".into(), "/c".into()],
                text: String::new(),
                app_id: None,
            })
        );
        let app = V::List(vec![kw("App"), kw("app"), t("org.x.Y.desktop")]);
        assert_eq!(
            drag_export(&app),
            Some(DropPayload::External {
                kind: DropKind::App,
                files: vec![],
                text: String::new(),
                app_id: Some("org.x.Y.desktop".into()),
            })
        );
        assert_eq!(drag_export(&V::List(vec![kw("App"), kw("app")])), None);
        assert_eq!(drag_export(&V::List(vec![kw("x"), kw("uri")])), None);
        assert_eq!(drag_type(&V::Number(1.0)), None);
    }

    #[test]
    fn drag_events_name_their_surface() {
        let s = SurfaceId(7);
        let at = LogicalPoint::new(3.0, 4.0);
        let payload = DropPayload::External {
            kind: DropKind::Files,
            files: vec![PathBuf::from("/tmp/a.png")],
            text: String::new(),
            app_id: None,
        };
        for e in [
            InputEvent::DragEnter {
                surface: s,
                at,
                kinds: vec![DropKind::Files, DropKind::Text],
            },
            InputEvent::DragMotion { surface: s, at },
            InputEvent::DragLeave { surface: s },
            InputEvent::DragDrop {
                surface: s,
                at,
                payload: payload.clone(),
            },
            InputEvent::DragDrop {
                surface: s,
                at,
                payload: DropPayload::Node(NodeId::new(2, 1)),
            },
        ] {
            assert_eq!(e.surface(), s);
            assert_eq!(e.clone(), e);
        }
        assert_ne!(payload, DropPayload::Node(NodeId::new(2, 1)));
    }
}
