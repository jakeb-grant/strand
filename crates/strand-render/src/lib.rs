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

mod flatten;
pub mod input;
mod layout;
mod markup;
mod raster;
mod renderer;
mod tree;

pub use input::{Flag, HitOnly, InputScene, Intent, NodeEvent, Router, WHEEL_STEP};
pub use layout::{Boxes, CH_EM, LIST_ROW_ESTIMATE, MAX_CONTENT_SIZE, RootSize, ScrollState};
pub use renderer::{
    BUSY_WINDOW, DAMAGE_HISTORY, NEW_TEXT_WAIT, QUERY_WAIT, RESIZE_WAIT, Renderer, TextBackend,
};
pub use tree::{Node, PropEntry, SceneError, SceneTree};
