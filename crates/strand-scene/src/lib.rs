//! Shared vocabulary between the logic side and the pixel side: ids,
//! geometry, colour, damage, the scene protocol and the `Painter` contract.
//!
//! See `docs/architecture.md`, "Contracts".

pub mod backend;
pub mod canvas;
pub mod color;
pub mod damage;
pub mod effect;
pub mod geometry;
pub mod id;
pub mod input;
pub mod motion;
pub mod paint;
pub mod protocol;
pub mod shader;
pub mod surface;
pub mod tokens;

pub use backend::{AdapterInfo, Backend, BackendChange, GpuStatus};
pub use canvas::DrawOp;

pub use color::{Color, LinearRgb, MIN_CONTRAST, Oklab, Oklch, REACH_MAX, luminance_reachable};
pub use damage::{Damage, MAX_RECTS};
pub use effect::{BlendMode, Bundled, Effect, Mask, ShaderInput, ShaderPass, ShaderRef};
pub use geometry::{LogicalPoint, LogicalRect, LogicalSize, Point, Rect, Scale, Size};
pub use id::{NodeId, NodeIdAllocator, SurfaceId};
pub use input::{
    AxisDelta, AxisSource, ButtonState, DropKind, DropPayload, InputEvent, KeyInput, Modifiers,
    drag_export, drag_type,
};
pub use motion::{Curve, Motion, Spring};
pub use paint::{
    BYTES_PER_PIXEL, BlurRegion, DragImage, PaintTarget, Painter, SurfacePose, TargetError,
};
pub use protocol::{
    Border, Corners, Easing, Font, GradientStop, Insets, Keyframes, Length, NodeKind, Paint, Prop,
    PropClass, PropValue, SceneDiff, SceneOp, Shadow, Transition,
};
pub use shader::{PRELUDE, ShaderCode, UniformSlot, UniformType};
pub use surface::{
    Anchor, CompositorCaps, Edge, Keyboard, Layer, Screens, SurfaceChange, SurfaceSpec, is_two_way,
};
pub use tokens::{
    BinOp, Channel, MAX_TOKEN_STEPS, TimeContext, TokenExpr, TokenMethod, TokenScope, TokenTable,
};
