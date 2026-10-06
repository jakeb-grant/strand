//! Shared vocabulary between the logic side and the pixel side: ids,
//! geometry, colour, damage, the scene protocol and the `Painter` contract.
//!
//! See `docs/architecture.md`, "Contracts".

pub mod color;
pub mod damage;
pub mod geometry;
pub mod id;
pub mod input;
pub mod paint;
pub mod protocol;
pub mod surface;
pub mod tokens;

pub use color::{Color, LinearRgb, Oklab, Oklch};
pub use damage::{Damage, MAX_RECTS};
pub use geometry::{LogicalPoint, LogicalRect, LogicalSize, Point, Rect, Scale, Size};
pub use id::{NodeId, NodeIdAllocator, SurfaceId};
pub use input::{AxisDelta, AxisSource, ButtonState, InputEvent, KeyInput, Modifiers};
pub use paint::{BYTES_PER_PIXEL, PaintTarget, Painter, TargetError};
pub use protocol::{
    Border, Corners, Easing, Font, GradientStop, Insets, Length, NodeKind, Paint, Prop, PropClass,
    PropValue, SceneDiff, SceneOp, Shadow, Transition,
};
pub use surface::{Anchor, Edge, Keyboard, Layer, Screens, SurfaceChange, SurfaceSpec};
pub use tokens::{BinOp, Channel, MAX_TOKEN_STEPS, TokenExpr, TokenMethod, TokenScope, TokenTable};
