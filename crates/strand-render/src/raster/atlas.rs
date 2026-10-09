//! The render thread's mirror of the text worker's glyph atlases.

use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::Scale;
use strand_text::{AtlasUpload, PageId};
use vello_cpu::Pixmap;
use vello_cpu::color::PremulRgba8;

/// The render thread's copy of the text worker's glyph atlases, as vello
/// pixmaps (white, premultiplied, coverage in every channel).
#[derive(Debug, Default)]
pub struct AtlasMirror {
    pub(super) pages: HashMap<(Scale, u32), MirrorPage>,
}

#[derive(Debug)]
pub(super) struct MirrorPage {
    pub(super) generation: u32,
    pub(super) pixmap: Arc<Pixmap>,
}

impl AtlasMirror {
    pub fn apply(&mut self, up: &AtlasUpload) {
        let size = up.page_size;
        let page = self
            .pages
            .entry((up.page.scale, up.page.index))
            .or_insert_with(|| MirrorPage {
                generation: up.page.generation,
                pixmap: Arc::new(Pixmap::new(size, size)),
            });
        if page.generation != up.page.generation
            || page.pixmap.width() != size
            || page.pixmap.height() != size
        {
            page.generation = up.page.generation;
            page.pixmap = Arc::new(Pixmap::new(size, size));
        }
        let pm = Arc::make_mut(&mut page.pixmap);
        let data = pm.data_mut();
        let (x, y, w, h) = (up.x as usize, up.y as usize, up.w as usize, up.h as usize);
        let stride = size as usize;
        if x + w > stride || y + h > stride || up.alpha.len() < w * h {
            return;
        }
        for row in 0..h {
            let src = &up.alpha[row * w..row * w + w];
            let dst = &mut data[(y + row) * stride + x..(y + row) * stride + x + w];
            for (d, &a) in dst.iter_mut().zip(src) {
                *d = PremulRgba8 {
                    r: a,
                    g: a,
                    b: a,
                    a,
                };
            }
        }
    }

    pub fn page(&self, id: PageId) -> Option<&Arc<Pixmap>> {
        self.pages
            .get(&(id.scale, id.index))
            .filter(|p| p.generation == id.generation)
            .map(|p| &p.pixmap)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Drops this scale's pages that are not in `live` (the worker trimmed
    /// or reset them; see `TextLayout::atlas_pages`).
    pub fn retain_pages(&mut self, scale: Scale, live: &[PageId]) {
        self.pages.retain(|(s, index), p| {
            *s != scale
                || live
                    .iter()
                    .any(|id| id.index == *index && id.generation == p.generation)
        });
    }

    /// Bytes of mirrored pixels held for `scale`.
    pub fn bytes(&self, scale: Scale) -> usize {
        self.pages
            .iter()
            .filter(|((s, _), _)| *s == scale)
            .map(|(_, p)| p.pixmap.width() as usize * p.pixmap.height() as usize * 4)
            .sum()
    }

    /// Drops pages of scales no surface uses any more.
    pub fn retain_scales(&mut self, keep: impl Fn(Scale) -> bool) {
        self.pages.retain(|(s, _), _| keep(*s));
    }
}
