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
