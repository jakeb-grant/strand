//! The scene protocol: one [`SceneDiff`] per logic tick, describing edits to
//! the retained tree the render thread owns.

use std::time::Duration;

use crate::color::Color;
use crate::id::NodeId;
use crate::tokens::{TokenExpr, TokenTable};

macro_rules! named_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident = $text:literal $(: $class:ident)? ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $( $(#[$vmeta])* $variant ),* }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            /// The snake_case name used in `.strand` source.
            pub const fn name(self) -> &'static str {
                match self { $($name::$variant => $text),* }
            }

            /// Looks a variant up by its source name.
            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($text => Some($name::$variant),)* _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }
    };
}

pub(crate) use named_enum;

named_enum! {
    /// What a scene node is. Components are expanded by the logic thread, so
    /// only built-in kinds reach the render thread.
    pub enum NodeKind {
        // Surfaces (roots).
        Bar = "bar",
        Panel = "panel",
        Osd = "osd",
        Popup = "popup",
        Lock = "lock",
        // Containers.
        Row = "row",
        Col = "col",
        Stack = "stack",
        Grid = "grid",
        Scroll = "scroll",
        List = "list",
        Split = "split",
        /// `start { }` region of a `split`.
        Start = "start",
        /// `center { }` region of a `split`.
        Center = "center",
        /// `end { }` region of a `split`.
        End = "end",
        Spacer = "spacer",
        Box = "box",
        // Widgets.
        Text = "text",
        Icon = "icon",
        Image = "image",
        Button = "button",
        Slider = "slider",
        Input = "input",
        Meter = "meter",
        Segmented = "segmented",
        Tooltip = "tooltip",
        Canvas = "canvas",
        // Structure.
        /// `pages current: page { page wifi {…} }`.
        Pages = "pages",
        Page = "page",
        // Effects, data and media.
        Arc = "arc",
        Graph = "graph",
        Spectrum = "spectrum",
        Particles = "particles",
        /// `effect lightning | sparks | shimmer | ripple | aurora { … }`.
        Effect = "effect",
        Shader = "shader",
        Svg = "svg",
        Lottie = "lottie",
        Thumbnail = "thumbnail",
        /// Per-letter animation: `letters { y: 2 * wave(1s, phase: index * 0.1) }`.
        Letters = "letters",
        /// Goo merge around siblings: `merge 10 { … }`.
        Merge = "merge",
    }
}

impl NodeKind {
    /// Surface kinds are the roots of the tree; each maps to one or more
    /// Wayland surfaces.
    pub const fn is_surface(self) -> bool {
        matches!(
            self,
            NodeKind::Bar | NodeKind::Panel | NodeKind::Osd | NodeKind::Popup | NodeKind::Lock
        )
    }
}

/// Which token spring `Transition::Default` resolves to for a prop.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PropClass {
    /// Movement and size: `$motion.spatial`.
    Spatial,
    /// Colour, opacity and other effects: `$motion.effects`.
    Effects,
    /// Cannot interpolate (fonts, text, keywords, booleans): snaps.
    Snap,
}

macro_rules! props {
    ($( $(#[$vmeta:meta])* $variant:ident = $text:literal : $class:ident ),* $(,)?) => {
        named_enum! {
            /// A node property. Names match the language's prop names.
            pub enum Prop { $( $(#[$vmeta])* $variant = $text ),* }
        }

        impl Prop {
            /// The default spring family for this prop.
            pub const fn class(self) -> PropClass {
                match self { $(Prop::$variant => PropClass::$class),* }
            }
        }
    };
}

props! {
    // Geometry and layout. Lengths are logical pixels.
    X = "x": Spatial,
    Y = "y": Spatial,
    Width = "width": Spatial,
    Height = "height": Spatial,
    Size = "size": Spatial,
    MinWidth = "min_width": Spatial,
    MaxWidth = "max_width": Spatial,
    MinHeight = "min_height": Spatial,
    MaxHeight = "max_height": Spatial,
    Pad = "pad": Spatial,
    Margin = "margin": Spatial,
    Gap = "gap": Spatial,
    Grow = "grow": Spatial,
    /// How much it gives up when its row or column is too small:
    /// `shrink: 0` keeps its size (default 1, as CSS).
    Shrink = "shrink": Spatial,
    /// Main-axis distribution of a container's children: `start`,
    /// `center`, `end`, `space_between`, `space_around`, `space_evenly`.
    Justify = "justify": Snap,
    Align = "align": Snap,
    Place = "place": Snap,
    Columns = "columns": Snap,
    Scale = "scale": Spatial,
    Rotate = "rotate": Spatial,
    // Surfaces.
    Edge = "edge": Snap,
    Anchor = "anchor": Snap,
    Layer = "layer": Snap,
    Keyboard = "keyboard": Snap,
    Screens = "screens": Snap,
    Open = "open": Snap,
    Attach = "attach": Snap,
    /// The declared name of a surface (`bar Top` → `"Top"`), as
    /// `PropValue::Text`. Set by the compiler, not written as a prop in
    /// source; the surface's layer-shell namespace is `strand-<name>`
    /// (see [`crate::SurfaceSpec`]).
    Name = "name": Snap,
    // Paint.
    Bg = "bg": Effects,
    Color = "color": Effects,
    Opacity = "opacity": Effects,
    Radius = "radius": Spatial,
    Corners = "corners": Snap,
    Border = "border": Effects,
    Shadow = "shadow": Effects,
    Blur = "blur": Effects,
    Clip = "clip": Snap,
    Glow = "glow": Effects,
    InnerShadow = "inner_shadow": Effects,
    Rim = "rim": Effects,
    Grain = "grain": Effects,
    Filter = "filter": Effects,
    Blend = "blend": Snap,
    Mask = "mask": Effects,
    Backdrop = "backdrop": Effects,
    Scrim = "scrim": Effects,
    Shape = "shape": Spatial,
    Stroke = "stroke": Effects,
    Track = "track": Effects,
    /// Fill of text glyphs or a graph: `fill: linear(…)`.
    Fill = "fill": Effects,
    BlurFallback = "blur_fallback": Snap,
    /// Stroke trim range: `trim: 0, progress`.
    Trim = "trim": Effects,
    /// Stroke dash pattern, a sub-prop of `stroke`:
    /// `stroke: 3, $accent { dash: 6, 4 }` (dash length, gap length).
    Dash = "dash": Effects,
    Cap = "cap": Snap,
    // Text.
    Text = "text": Snap,
    Font = "font": Snap,
    Weight = "weight": Snap,
    Ellipsis = "ellipsis": Snap,
    MaxLines = "max_lines": Snap,
    Markup = "markup": Snap,
    Marks = "marks": Snap,
    MarkColor = "mark_color": Effects,
    Roll = "roll": Snap,
    TextStroke = "text_stroke": Effects,
    // Widgets and input.
    Value = "value": Spatial,
    Placeholder = "placeholder": Snap,
    /// What an `input` holds: `type: password` masks it.
    InputType = "type": Snap,
    Options = "options": Snap,
    Source = "source": Snap,
    Fit = "fit": Snap,
    Focus = "focus": Snap,
    Nav = "nav": Snap,
    Hit = "hit": Snap,
    Tooltip = "tooltip": Snap,
    Drag = "drag": Snap,
    Morph = "morph": Snap,
    Stagger = "stagger": Snap,
    Wave = "wave": Spatial,
    Jelly = "jelly": Effects,
    Parallax = "parallax": Spatial,
    Tilt = "tilt": Spatial,
    // Poses, structure and motion. `enter`/`exit` hold a
    // [`PropValue::Pose`] (or a preset keyword such as `popin`); render
    // stores them now and plays them in M2.
    Enter = "enter": Snap,
    Exit = "exit": Snap,
    /// `transition: wipe(left) | disc | dissolve | pixelate`.
    Transition = "transition": Snap,
    /// `pages current: page`.
    Current = "current": Snap,
    /// `play shake` (keyframes).
    Play = "play": Snap,
    // Data, effects and media nodes.
    /// `arc { sweep: 270deg }`.
    Sweep = "sweep": Spatial,
    Bars = "bars": Snap,
    Smooth = "smooth": Snap,
    Style = "style": Snap,
    History = "history": Snap,
    Rate = "rate": Snap,
    Life = "life": Snap,
    Sprite = "sprite": Snap,
    Speed = "speed": Snap,
    /// `canvas { draw: (c) => … }`.
    Draw = "draw": Snap,
    // Tokens.
    /// Token overrides for this node and its subtree, as a
    /// [`PropValue::Tokens`] table: `set { $surface: $surface.alpha(0.5) }`
    /// and a component's `tokens { radius: $radius.lg }` (as
    /// `Toast.radius`) both lower to it. See [`crate::TokenScope`].
    Tokens = "tokens": Snap,
}

/// A length in logical pixels or relative to a reference.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Length {
    Px(f32),
    /// Percent of the parent's corresponding dimension (`40%` is `40.0`).
    Percent(f32),
    /// Multiples of the width of `0` in the node's font (`4ch`).
    Ch(f32),
    Auto,
}

/// Four edge lengths in logical pixels, for `pad` and `margin`.
///
/// The comma shorthand (`margin: 8, 8, 0`, `pad: 0, $space.3`) arrives
/// as a `PropValue::List` of one to four numbers, expanded like CSS by
/// [`Insets::from_values`] / [`PropValue::insets`]; a list may hold token
/// references, which [`crate::TokenScope::resolve`] resolves in place.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Insets {
    pub const fn all(v: f32) -> Self {
        Self {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }

    /// CSS expansion of one to four values: `a` is all sides, `a, b` is
    /// vertical then horizontal, `a, b, c` is top, horizontal, bottom, and
    /// four values run clockwise from the top.
    pub fn from_values(v: &[f32]) -> Option<Self> {
        let (top, right, bottom, left) = match *v {
            [a] => (a, a, a, a),
            [a, b] => (a, b, a, b),
            [a, b, c] => (a, b, c, b),
            [a, b, c, d] => (a, b, c, d),
            _ => return None,
        };
        Some(Self {
            top,
            right,
            bottom,
            left,
        })
    }
}

/// Per-corner radii in logical pixels, clockwise from top-left
/// (`radius: 14, 14, 0, 0`).
///
/// `radius: full` (a pill or circle) is [`Corners::FULL`]: an infinite
/// radius, which shrinks like CSS to half the shorter side. Render also
/// accepts `PropValue::Keyword("full")` for the `radius` prop, and a
/// `Length::Percent` radius is a percentage of the shorter side.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Corners {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl Corners {
    pub const fn all(r: f32) -> Self {
        Self {
            top_left: r,
            top_right: r,
            bottom_right: r,
            bottom_left: r,
        }
    }

    /// `radius: full`.
    pub const FULL: Corners = Corners::all(f32::INFINITY);

    /// CSS `border-radius` expansion of one to four values (`radius: 14,
    /// 14, 0, 0` runs clockwise from top-left; `a, b` is top-left and
    /// bottom-right, then the other two; `a, b, c` is top-left, the
    /// top-right/bottom-left pair, bottom-right).
    pub fn from_values(v: &[f32]) -> Option<Self> {
        let (top_left, top_right, bottom_right, bottom_left) = match *v {
            [a] => (a, a, a, a),
            [a, b] => (a, b, a, b),
            [a, b, c] => (a, b, c, b),
            [a, b, c, d] => (a, b, c, d),
            _ => return None,
        };
        Some(Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.top_left <= 0.0
            && self.top_right <= 0.0
            && self.bottom_right <= 0.0
            && self.bottom_left <= 0.0
    }
}

/// A colour stop of a gradient; `offset` is in `0..=1`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
}

/// How an area is filled.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Color),
    /// `linear(45deg, …)`; the angle follows CSS (0deg points up, clockwise).
    Linear {
        angle: f32,
        stops: Vec<GradientStop>,
    },
    /// `radial(…)` from the centre to the farthest corner.
    Radial {
        stops: Vec<GradientStop>,
    },
    /// `conic(from: 90deg, …)`.
    Conic {
        from: f32,
        stops: Vec<GradientStop>,
    },
}

impl From<Color> for Paint {
    fn from(c: Color) -> Self {
        Paint::Solid(c)
    }
}

/// `border: 1, $border`.
#[derive(Clone, Debug, PartialEq)]
pub struct Border {
    pub width: f32,
    pub paint: Paint,
}

/// One drop shadow: `0 2px 8px $shadow.alpha(0.25)` is `x: 0, y: 2, blur: 8`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
}

/// `font: "Inter" 13px 500`.
#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    /// A family name or a CSS-style comma-separated list with generic
    /// fallbacks (`"Inter, sans-serif"`).
    pub family: String,
    /// Size in logical pixels.
    pub size: f32,
    /// CSS weight, 1–1000 (400 regular, 700 bold).
    pub weight: u16,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: "sans-serif".into(),
            size: 13.0,
            weight: 400,
        }
    }
}

/// A typed prop value. Values bound to tokens arrive as
/// [`PropValue::Token`] and are evaluated by the render thread against the
/// current [`TokenTable`] (see [`TokenTable::resolve`]).
#[derive(Clone, Debug, PartialEq)]
pub enum PropValue {
    /// Reverts the prop to its default (a `when` stopped applying and no
    /// base value exists).
    Unset,
    Bool(bool),
    Number(f32),
    Length(Length),
    Insets(Insets),
    Corners(Corners),
    Color(Color),
    Paint(Paint),
    Border(Border),
    /// A shadow list; an empty list means no shadow.
    Shadow(Vec<Shadow>),
    Text(String),
    Font(Font),
    /// An enum value or keyword such as `center`, `end`, `squircle`.
    Keyword(String),
    /// Degrees.
    Angle(f32),
    Duration(Duration),
    List(Vec<PropValue>),
    /// A token reference or expression (`$accent`, `$fg.alpha(0.65)`,
    /// `border: 1, $border`), evaluated by render every frame.
    Token(TokenExpr),
    /// A spring or timed curve as a value (`$motion.spatial:
    /// spring(700, 0.9)`); `Transition::Default` resolves through these.
    Transition(Transition),
    /// An `enter { … }` / `exit { … }` pose: the props a node animates in
    /// from or out to.
    Pose(Vec<(Prop, PropValue)>),
    /// Token overrides for a subtree (the value of [`Prop::Tokens`]).
    Tokens(Box<TokenTable>),
    /// A call-shaped value: `hit: grow(6)`, `backdrop: blur(16)`,
    /// `filter: grayscale(1)` (a chain of filters is a `List` of calls),
    /// `filter: tint($accent)`, `transition: wipe(left)`. `name` is the
    /// function as written; arguments are typed values and may hold
    /// tokens. Render ignores calls it does not implement yet. Named
    /// arguments (`conic(from: 90deg, …)`) are not calls: they lower to
    /// their typed value (`Paint::Conic`).
    Call {
        name: String,
        args: Vec<PropValue>,
    },
    /// Another scene node, named by its `id:` (`nav: results` routes arrow
    /// keys from an `input` to that `list`).
    Node(crate::id::NodeId),
}

impl PropValue {
    /// True if a token reference sits anywhere inside the value.
    pub fn has_tokens(&self) -> bool {
        match self {
            PropValue::Token(_) => true,
            PropValue::List(items) | PropValue::Call { args: items, .. } => {
                items.iter().any(PropValue::has_tokens)
            }
            PropValue::Pose(props) => props.iter().any(|(_, v)| v.has_tokens()),
            _ => false,
        }
    }

    /// A plain number: `Number` or `Length::Px`.
    pub fn as_number(&self) -> Option<f32> {
        match self {
            PropValue::Number(n) | PropValue::Length(Length::Px(n)) => Some(*n),
            _ => None,
        }
    }

    /// Insets from `Insets`, one number, or a `List` of one to four numbers
    /// (the comma shorthand, expanded like CSS). Resolve tokens first.
    pub fn insets(&self) -> Option<Insets> {
        match self {
            PropValue::Insets(i) => Some(*i),
            PropValue::List(items) => {
                let v: Option<Vec<f32>> = items.iter().map(PropValue::as_number).collect();
                Insets::from_values(&v?)
            }
            v => v.as_number().map(Insets::all),
        }
    }

    /// Every colour inside the value, depth first in field order. This is
    /// the order [`TokenExpr::Template`] fills colours in.
    pub fn colors_mut(&mut self) -> Vec<&mut Color> {
        let mut out = Vec::new();
        self.collect_colors(&mut out);
        out
    }

    fn collect_colors<'a>(&'a mut self, out: &mut Vec<&'a mut Color>) {
        fn paint<'a>(p: &'a mut Paint, out: &mut Vec<&'a mut Color>) {
            match p {
                Paint::Solid(c) => out.push(c),
                Paint::Linear { stops, .. }
                | Paint::Radial { stops }
                | Paint::Conic { stops, .. } => out.extend(stops.iter_mut().map(|s| &mut s.color)),
            }
        }
        match self {
            PropValue::Color(c) => out.push(c),
            PropValue::Paint(p) => paint(p, out),
            PropValue::Border(b) => paint(&mut b.paint, out),
            PropValue::Shadow(list) => out.extend(list.iter_mut().map(|s| &mut s.color)),
            PropValue::List(items) | PropValue::Call { args: items, .. } => {
                for v in items {
                    v.collect_colors(out);
                }
            }
            PropValue::Pose(props) => {
                for (_, v) in props {
                    v.collect_colors(out);
                }
            }
            _ => {}
        }
    }
}

/// Named easing curves are cubic béziers; see [`Easing::named`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Easing {
    Linear,
    /// CSS `cubic-bezier(x1, y1, x2, y2)`.
    Bezier {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
}

impl Easing {
    /// The curve `~ 200ms` uses when no curve is named.
    pub const STANDARD: Easing = Easing::Bezier {
        x1: 0.2,
        y1: 0.0,
        x2: 0.0,
        y2: 1.0,
    };

    /// Looks up a named curve for `~ ease(name, T)`.
    pub fn named(name: &str) -> Option<Easing> {
        let b = |x1, y1, x2, y2| Some(Easing::Bezier { x1, y1, x2, y2 });
        match name {
            "linear" => Some(Easing::Linear),
            "standard" => Some(Self::STANDARD),
            "ease" => b(0.25, 0.1, 0.25, 1.0),
            "ease_in" | "in" => b(0.42, 0.0, 1.0, 1.0),
            "ease_out" | "out" => b(0.0, 0.0, 0.58, 1.0),
            "ease_in_out" | "in_out" => b(0.42, 0.0, 0.58, 1.0),
            "in_back" => b(0.36, 0.0, 0.66, -0.56),
            "out_back" => b(0.34, 1.56, 0.64, 1.0),
            "in_out_back" => b(0.68, -0.6, 0.32, 1.6),
            "emphasized" => b(0.2, 0.0, 0.0, 1.0),
            "emphasized_decelerate" => b(0.05, 0.7, 0.1, 1.0),
            "emphasized_accelerate" => b(0.3, 0.0, 0.8, 0.15),
            _ => None,
        }
    }
}

/// How a prop moves to its new value, matching `~` in the language.
/// [`crate::TokenScope::transition`] turns `Default` and `Token` into a
/// concrete curve.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Transition {
    /// The token spring for the prop's class (see [`Prop::class`]).
    #[default]
    Default,
    /// `~ $motion.bouncy`: a token path (without the `$`) holding a
    /// [`PropValue::Transition`], resolved like `Default` so a theme swap
    /// reaches props that named it.
    Token(String),
    /// `~ spring(700, 0.9)`: stiffness and damping ratio.
    Spring { stiffness: f32, damping: f32 },
    /// `~ 200ms` or `~ ease(out_back, 300ms)` or `~ bezier(…)`.
    Duration { duration: Duration, easing: Easing },
    /// `~ instant`.
    Instant,
}

/// One edit to the retained tree.
#[derive(Clone, Debug, PartialEq)]
pub enum SceneOp {
    /// Creates a node as child `index` of `parent` (`None` for a surface
    /// root); indices past the end append. A surface-kind node created
    /// under a parent (a `popup` inside a `bar`) is its own surface root:
    /// it does not paint into its parent's surface.
    Create {
        id: NodeId,
        kind: NodeKind,
        parent: Option<NodeId>,
        index: u32,
    },
    /// Removes a node and its subtree; render plays `exit` before unmounting.
    /// The id is dead as soon as the op applies: logic may reuse its slot
    /// with a new generation in the same diff. (From M2 an exiting subtree
    /// lives on as a render-side ghost outside the id-indexed slots, so
    /// slot reuse never waits for an exit animation.)
    Remove { id: NodeId },
    /// Re-parents or reorders a node. `index` is the position among the
    /// new parent's children *after* the node has been detached from its
    /// old place (so moving the first of three children to the end is
    /// index 2); indices past the end append.
    Move {
        id: NodeId,
        parent: Option<NodeId>,
        index: u32,
    },
    SetProp {
        id: NodeId,
        prop: Prop,
        value: PropValue,
        transition: Transition,
    },
    /// Replaces the global token table. Palette roots move to their new
    /// values with `transition` (from M2; `Default` is `$motion.effects`
    /// of the new table); logic sends `Instant` for the table it boots
    /// with, so the first frame never shows default colours. Derived
    /// tokens are re-evaluated from the roots every frame either way.
    SetTokens {
        table: TokenTable,
        transition: Transition,
    },
}

/// One logic tick's worth of ordered ops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneDiff {
    pub ops: Vec<SceneOp>,
}

impl SceneDiff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, op: SceneOp) -> &mut Self {
        self.ops.push(op);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Convenience for [`SceneOp::Create`].
    pub fn create(
        &mut self,
        id: NodeId,
        kind: NodeKind,
        parent: Option<NodeId>,
        index: u32,
    ) -> &mut Self {
        self.push(SceneOp::Create {
            id,
            kind,
            parent,
            index,
        })
    }

    /// Convenience for [`SceneOp::SetTokens`].
    pub fn set_tokens(&mut self, table: TokenTable, transition: Transition) -> &mut Self {
        self.push(SceneOp::SetTokens { table, transition })
    }

    /// Convenience for [`SceneOp::SetProp`] with [`Transition::Default`].
    pub fn set(&mut self, id: NodeId, prop: Prop, value: PropValue) -> &mut Self {
        self.push(SceneOp::SetProp {
            id,
            prop,
            value,
            transition: Transition::Default,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for p in Prop::ALL {
            assert_eq!(Prop::from_name(p.name()), Some(*p));
        }
        for k in NodeKind::ALL {
            assert_eq!(NodeKind::from_name(k.name()), Some(*k));
        }
        assert_eq!(Prop::from_name("bg"), Some(Prop::Bg));
        assert_eq!(Prop::from_name("background"), None);
        assert!(NodeKind::Bar.is_surface());
        assert!(!NodeKind::Row.is_surface());
        // The design catalogue's props and kinds are all known.
        for p in [
            "fill",
            "text_stroke",
            "blur_fallback",
            "enter",
            "exit",
            "sweep",
            "transition",
            "tilt",
        ] {
            assert!(Prop::from_name(p).is_some(), "{p}");
        }
        for k in [
            "arc",
            "graph",
            "spectrum",
            "particles",
            "svg",
            "lottie",
            "thumbnail",
            "pages",
            "page",
            "letters",
            "merge",
            "effect",
            "shader",
        ] {
            assert!(NodeKind::from_name(k).is_some(), "{k}");
        }
        let mut names: Vec<_> = Prop::ALL.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Prop::ALL.len(), "prop names are unique");
    }

    #[test]
    fn colors_are_visited_in_field_order() {
        let mut v = PropValue::List(vec![
            PropValue::Border(Border {
                width: 1.0,
                paint: Paint::Solid(Color::BLACK),
            }),
            PropValue::Paint(Paint::Linear {
                angle: 0.0,
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: Color::WHITE,
                    },
                    GradientStop {
                        offset: 1.0,
                        color: Color::TRANSPARENT,
                    },
                ],
            }),
        ]);
        let got: Vec<Color> = v.colors_mut().into_iter().map(|c| *c).collect();
        assert_eq!(got, [Color::BLACK, Color::WHITE, Color::TRANSPARENT]);
    }

    #[test]
    fn prop_classes_follow_design() {
        // $motion.spatial for movement and size, $motion.effects for colour
        // and opacity; fonts snap.
        assert_eq!(Prop::Width.class(), PropClass::Spatial);
        assert_eq!(Prop::X.class(), PropClass::Spatial);
        assert_eq!(Prop::Bg.class(), PropClass::Effects);
        assert_eq!(Prop::Opacity.class(), PropClass::Effects);
        assert_eq!(Prop::Font.class(), PropClass::Snap);
    }

    #[test]
    fn easing_names() {
        assert!(matches!(
            Easing::named("out_back"),
            Some(Easing::Bezier { y1, .. }) if y1 > 1.0
        ));
        assert_eq!(Easing::named("nope"), None);
    }

    #[test]
    fn diff_builder() {
        let id = NodeId::new(0, 0);
        let mut d = SceneDiff::new();
        d.create(id, NodeKind::Bar, None, 0)
            .set(id, Prop::Bg, PropValue::Color(Color::BLACK));
        assert_eq!(d.ops.len(), 2);
        assert!(matches!(
            d.ops[1],
            SceneOp::SetProp {
                transition: Transition::Default,
                ..
            }
        ));
    }
}
