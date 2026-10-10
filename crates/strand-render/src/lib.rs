//! Render thread.
//!
//! Owns every spring, resolves the token graph each animating frame, runs
//! taffy layout, tracks damage (up to 8 rects per frame) and paints the scene
//! IR with vello_cpu into shared memory, promoting to the GPU only for heavy
//! animation.
//!
//! See `docs/design.md`, "Layout, animation and input" and "Rendering,
//! performance and memory budget". vello_cpu and damage land in M0; layout,
//! springs and tokens in M2.
//!
//! M0 pipeline: [`Renderer::apply`] edits the retained [`SceneTree`]; each
//! [`Painter::paint`](strand_scene::Painter::paint) flattens the surface's
//! subtree to a display list with a bounds + signature record per node,
//! diffs the records against the previous frame to get exact damage, widens
//! it by the buffer's age and rasterises only inside it.

mod anim;
mod backdrop;
mod cache;
mod canvas;
mod clock;
mod effects;
mod fillet;
mod flatten;
pub mod image;
pub mod input;
mod layers;
mod layout;
pub mod lock_fallback;
mod markup;
mod media;
mod offscreen;
mod pose;
#[cfg(feature = "gpu")]
pub mod promote;
mod raster;
mod renderer;
mod shapes;
mod time;
mod tree;
pub mod widgets;

pub use anim::PageSwap;
pub use cache::{MAX_ENTRY_BYTES, PAINT_CACHE_BYTES};
pub use clock::Rate;
pub use flatten::BLUR_TINT;
pub use input::{DragView, Flag, HitOnly, InputScene, Intent, NodeEvent, Router, WHEEL_STEP};
pub use layout::{
    Boxes, CH_EM, FLING_DECAY, LAYOUT_OVERSCAN, LIST_ROW_ESTIMATE, ListBox, ListWindow,
    MAX_CONTENT_SIZE, RootSize, ScrollState, WINDOW_NEED, WINDOW_OVERSCAN,
};
pub use media::thumbnail::Frame as ThumbnailFrame;
pub use offscreen::{OFFSCREEN_BYTES, RasterProps, RasterSource};
#[cfg(feature = "gpu")]
pub use renderer::GPU_WAIT;
pub use renderer::{
    BUSY_WINDOW, DAMAGE_HISTORY, EXIT_STALL, MAX_GHOSTS_PER_PARENT, NEW_TEXT_WAIT, QUERY_WAIT,
    RESIZE_WAIT, Renderer, TOOLTIP_DELAY, TextBackend,
};
pub use renderer::{FeedDemand, FeedKind, ListFrames, ScrollInput, ScrollKind, THUMBNAIL_STEP};
pub use tree::{Node, PropEntry, SceneError, SceneTree};
