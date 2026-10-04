//! Text worker thread.
//!
//! parley shapes text off the render thread; swash rasterises into LRU glyph
//! atlases, one per output scale, so mixed-DPI setups stay sharp.
//!
//! See `docs/design.md`, "Rendering, performance and memory budget".
