//! (M4) Media and data nodes (design.md, "Generative, data-driven and
//! media"): animated images (through the image store), and the CPU
//! raster sources of `graph` and `spectrum`, attached to their nodes when
//! logic creates them ([`Media`]).

pub mod animated;
pub mod graph;
pub mod spectrum;

use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::{NodeId, NodeKind, SceneDiff, SceneOp};
use vello_cpu::color::PremulRgba8;
use vello_cpu::{
    Pixmap, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

/// The media nodes a diff creates, by kind: what [`Media`] attaches.
pub(crate) fn created(diff: &SceneDiff) -> Vec<(NodeId, NodeKind)> {
    diff.ops
        .iter()
        .filter_map(|op| match op {
            SceneOp::Create { id, kind, .. } if is_media(*kind) => Some((*id, *kind)),
            _ => None,
        })
        .collect()
}

/// The kinds drawn by a source of their own.
pub(crate) fn is_media(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Graph | NodeKind::Spectrum)
}

/// One media node's source, typed for what feeds it.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Graph(Arc<graph::GraphSource>),
    Spectrum(Arc<spectrum::SpectrumSource>),
}

impl Source {
    pub(crate) fn new(kind: NodeKind) -> Option<Source> {
        Some(match kind {
            NodeKind::Graph => Source::Graph(Arc::default()),
            NodeKind::Spectrum => Source::Spectrum(Arc::default()),
            _ => return None,
        })
    }

    /// As the renderer's raster seam takes it.
    pub(crate) fn raster(&self) -> Arc<dyn crate::offscreen::RasterSource> {
        match self {
            Source::Graph(g) => g.clone(),
            Source::Spectrum(s) => s.clone(),
        }
    }
}

/// The media nodes' sources by node.
#[derive(Debug, Default)]
pub struct Media {
    pub(crate) sources: HashMap<NodeId, Source>,
    /// The demand last handed out ([`crate::Renderer::take_feed_demand`]).
    pub(crate) demand: Vec<crate::FeedDemand>,
}

/// Draws vector content with vello_cpu (single-threaded, as everywhere in
/// render) into a `w × h` premultiplied RGBA buffer.
pub(crate) fn vector(
    pixels: &mut [PremulRgba8],
    w: u32,
    h: u32,
    draw: impl FnOnce(&mut RenderContext),
) {
    let (Ok(w16), Ok(h16)) = (u16::try_from(w), u16::try_from(h)) else {
        return;
    };
    if w16 == 0 || h16 == 0 {
        return;
    }
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(w16, h16, settings);
    draw(&mut ctx);
    ctx.flush();
    let mut pm = Pixmap::new(w16, h16);
    let mut resources = Resources::new();
    ctx.render_with(
        pm.as_mut(),
        &mut resources,
        RasterizerSettings {
            target_init: TargetInit::SrcOver,
            render_mode: RenderMode::OptimizeQuality,
            ..RasterizerSettings::default()
        },
    );
    let n = pixels.len().min(pm.data().len());
    pixels[..n].copy_from_slice(&pm.data()[..n]);
}
