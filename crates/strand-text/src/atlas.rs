//! Per-scale glyph atlases: alpha-mask pages packed with a shelf allocator,
//! bounded by a page count and evicted least-recently-used, one page at a
//! time.
//!
//! The atlas lives on the text worker. The render thread keeps a mirror of
//! each page, fed by the [`AtlasUpload`]s every [`crate::TextLayout`]
//! carries. A page is only reset while no delivered layout references it:
//! layouts hold a [`PageLease`] for every page their glyphs sit on, and the
//! worker skips leased pages when choosing a victim.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use strand_scene::Scale;

/// Identifies one page of one per-scale atlas. `generation` changes every
/// time the page is (re)created or reset, and is unique in the process, so
/// a mirror can tell stale contents apart even across dropped atlases.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageId {
    pub scale: Scale,
    pub index: u32,
    pub generation: u32,
}

/// Where a glyph's alpha mask sits in its page.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AtlasSlot {
    pub page: PageId,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// New pixels for a page region. Pages are square, `page_size` pixels on a
/// side (larger than [`AtlasConfig::page_size`] for a page dedicated to an
/// oversized glyph), one byte of coverage per pixel, rows of `w` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct AtlasUpload {
    pub page: PageId,
    pub page_size: u16,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub alpha: Vec<u8>,
}

impl std::fmt::Debug for AtlasUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtlasUpload")
            .field("page", &self.page)
            .field("rect", &(self.x, self.y, self.w, self.h))
            .finish_non_exhaustive()
    }
}

/// Keeps a page from being reset while a layout that uses it is alive.
#[derive(Clone, Debug)]
pub struct PageLease(#[allow(dead_code)] Arc<PageId>);

/// Atlas sizing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AtlasConfig {
    /// Side of a square page in pixels.
    pub page_size: u16,
    /// Pages per scale before least-recently-used pages are evicted. Pages
    /// beyond this are only allocated while every page is leased.
    pub max_pages: usize,
}

/// Largest page side; a glyph mask bigger than this minus one pixel of
/// padding is not drawn (font sizes are capped well below it).
pub const MAX_PAGE_SIZE: u16 = 2048;

static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

fn next_generation() -> u32 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self {
            page_size: 256,
            max_pages: 4,
        }
    }
}

/// Key of a rasterised glyph within one scale's atlas.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    pub font_id: u64,
    pub font_index: u32,
    pub glyph: u32,
    /// Pixel size as `f32` bits.
    pub size_bits: u32,
    /// Horizontal subpixel bucket, `0..SUBPIXEL_STEPS`.
    pub subpixel: u8,
    pub embolden: bool,
    pub skew: i8,
    /// Hash of the normalised variation coordinates.
    pub coords: u64,
}

/// A rasterised glyph: its slot (if it has pixels) and its bitmap offset
/// from the pen position (left, top: y grows down, so `top` is the distance
/// above the baseline).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedGlyph {
    pub slot: Option<AtlasSlot>,
    pub left: i32,
    pub top: i32,
}

#[derive(Debug)]
struct Shelf {
    y: u16,
    h: u16,
    x: u16,
}

#[derive(Debug)]
struct Page {
    id: PageId,
    /// Side in pixels.
    size: u16,
    lease: Arc<PageId>,
    shelves: Vec<Shelf>,
    next_y: u16,
    last_used: u64,
}

impl Page {
    fn new(scale: Scale, index: u32, size: u16) -> Self {
        let id = PageId {
            scale,
            index,
            generation: next_generation(),
        };
        Self {
            id,
            size,
            lease: Arc::new(id),
            shelves: Vec::new(),
            next_y: 0,
            last_used: 0,
        }
    }

    fn leased(&self) -> bool {
        Arc::strong_count(&self.lease) > 1
    }

    fn reset(&mut self, size: u16) {
        self.id.generation = next_generation();
        self.size = size;
        self.lease = Arc::new(self.id);
        self.shelves.clear();
        self.next_y = 0;
    }

    /// Shelf packing with one pixel of padding on the right and bottom.
    fn alloc(&mut self, w: u16, h: u16) -> Option<(u16, u16)> {
        let size = self.size;
        let (pw, ph) = (w.checked_add(1)?, h.checked_add(1)?);
        if pw > size || ph > size {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, s) in self.shelves.iter().enumerate() {
            let fits = s.h >= ph && size - s.x >= pw;
            // Avoid wasting tall shelves on short glyphs.
            let snug = s.h <= ph.saturating_mul(3) / 2 + 1;
            if fits && snug && best.is_none_or(|b| self.shelves[b].h > s.h) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let s = &mut self.shelves[i];
            let pos = (s.x, s.y);
            s.x += pw;
            return Some(pos);
        }
        if size - self.next_y < ph {
            return None;
        }
        let y = self.next_y;
        self.next_y += ph;
        self.shelves.push(Shelf { y, h: ph, x: pw });
        Some((0, y))
    }
}

/// One output scale's atlas.
#[derive(Debug)]
pub(crate) struct GlyphAtlas {
    scale: Scale,
    config: AtlasConfig,
    pages: Vec<Page>,
    glyphs: HashMap<GlyphKey, CachedGlyph>,
}

impl GlyphAtlas {
    pub fn new(scale: Scale, config: AtlasConfig) -> Self {
        Self {
            scale,
            config,
            pages: Vec::new(),
            glyphs: HashMap::new(),
        }
    }

    pub fn get(&self, key: &GlyphKey) -> Option<CachedGlyph> {
        let g = self.glyphs.get(key).copied()?;
        // Entries are removed on page reset, so a hit is always current.
        debug_assert!(
            g.slot
                .is_none_or(|s| self.pages[s.page.index as usize].id == s.page)
        );
        Some(g)
    }

    pub fn insert(&mut self, key: GlyphKey, glyph: CachedGlyph) {
        self.glyphs.insert(key, glyph);
    }

    /// Marks a page as used by the request stamped `now` and returns a lease
    /// for it.
    pub fn touch(&mut self, page: PageId, now: u64) -> PageLease {
        let p = &mut self.pages[page.index as usize];
        p.last_used = now;
        PageLease(p.lease.clone())
    }

    /// Finds room for a `w × h` mask. Pages used by the current request
    /// (stamped `now`) and leased pages are never evicted. A mask too big
    /// for a regular page gets a page of its own, sized to fit, which is
    /// leased and evicted like any other. `None` if nothing fits; such
    /// failures are not cached, so a later request retries.
    pub fn allocate(&mut self, w: u16, h: u16, now: u64) -> Option<AtlasSlot> {
        let needed = w.max(h).checked_add(1)?;
        if needed > MAX_PAGE_SIZE {
            return None;
        }
        let regular = self.config.page_size;
        let size = if needed <= regular {
            // Most recently used first: keeps hot glyphs together.
            let mut order: Vec<usize> = (0..self.pages.len()).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(self.pages[i].last_used));
            for i in order {
                if let Some((x, y)) = self.pages[i].alloc(w, h) {
                    return Some(self.slot(i, x, y, w, h, now));
                }
            }
            regular
        } else {
            // Round up so a page can be reused for similar sizes.
            needed.div_ceil(64).saturating_mul(64).min(MAX_PAGE_SIZE)
        };
        let index = if self.pages.len() < self.config.max_pages {
            None
        } else {
            self.pages
                .iter()
                .enumerate()
                .filter(|(_, p)| p.last_used != now && !p.leased())
                .min_by_key(|(_, p)| p.last_used)
                .map(|(i, _)| i)
        };
        let i = match index {
            Some(i) => {
                let old = self.pages[i].id;
                self.glyphs
                    .retain(|_, g| g.slot.is_none_or(|s| s.page.index != old.index));
                self.pages[i].reset(size);
                i
            }
            None => {
                let i = self.pages.len();
                self.pages.push(Page::new(self.scale, i as u32, size));
                i
            }
        };
        let (x, y) = self.pages[i].alloc(w, h)?;
        Some(self.slot(i, x, y, w, h, now))
    }

    /// Frees pages above `max_pages` that were only allocated while every
    /// page was leased, once they are no longer leased. Pages are removed
    /// from the end so indices stay stable.
    pub fn trim(&mut self, now: u64) {
        while self.pages.len() > self.config.max_pages {
            let Some(last) = self.pages.last() else {
                break;
            };
            if last.leased() || last.last_used == now {
                break;
            }
            let index = last.id.index;
            self.glyphs
                .retain(|_, g| g.slot.is_none_or(|s| s.page.index != index));
            self.pages.pop();
        }
    }

    /// Side of the page `page` lives on.
    pub fn page_size_of(&self, page: PageId) -> u16 {
        self.pages
            .get(page.index as usize)
            .map_or(self.config.page_size, |p| p.size)
    }

    fn slot(&mut self, i: usize, x: u16, y: u16, w: u16, h: u16, now: u64) -> AtlasSlot {
        self.pages[i].last_used = now;
        AtlasSlot {
            page: self.pages[i].id,
            x,
            y,
            w,
            h,
        }
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas(max_pages: usize) -> GlyphAtlas {
        GlyphAtlas::new(
            Scale::ONE,
            AtlasConfig {
                page_size: 32,
                max_pages,
            },
        )
    }

    fn key(glyph: u32) -> GlyphKey {
        GlyphKey {
            font_id: 1,
            font_index: 0,
            glyph,
            size_bits: 0,
            subpixel: 0,
            embolden: false,
            skew: 0,
            coords: 0,
        }
    }

    #[test]
    fn slots_do_not_overlap() {
        let mut a = atlas(1);
        let mut slots = Vec::new();
        // Same request (time 1) throughout: the full page cannot be evicted,
        // so the atlas spills onto a second page.
        while let Some(s) = a.allocate(5, 7, 1) {
            if s.page.index != 0 {
                break;
            }
            slots.push(s);
        }
        // 32 / 6 = 5 columns, 32 / 8 = 4 shelves.
        assert_eq!(slots.len(), 20);
        for (i, s) in slots.iter().enumerate() {
            assert!(s.x + s.w <= 32 && s.y + s.h <= 32);
            for t in &slots[i + 1..] {
                let apart =
                    s.x + s.w <= t.x || t.x + t.w <= s.x || s.y + s.h <= t.y || t.y + t.h <= s.y;
                assert!(apart, "{s:?} overlaps {t:?}");
            }
        }
    }

    #[test]
    fn lru_page_is_evicted_unless_leased() {
        let mut a = atlas(2);
        // Fill page 0 at time 1 and page 1 at time 2.
        let s0 = a.allocate(30, 30, 1).unwrap();
        a.insert(
            key(0),
            CachedGlyph {
                slot: Some(s0),
                left: 0,
                top: 0,
            },
        );
        let s1 = a.allocate(30, 30, 2).unwrap();
        assert_ne!(s0.page.index, s1.page.index);
        a.touch(s1.page, 2);
        // Time 3 needs a page: page 0 is least recently used.
        let s2 = a.allocate(30, 30, 3).unwrap();
        assert_eq!(s2.page.index, s0.page.index);
        assert_ne!(s2.page.generation, s0.page.generation);
        assert!(a.get(&key(0)).is_none(), "evicted glyphs leave the cache");

        // Lease both pages: the atlas must grow past max_pages instead.
        let _l1 = a.touch(s1.page, 3);
        let _l2 = a.touch(s2.page, 3);
        let s3 = a.allocate(30, 30, 4).unwrap();
        assert_eq!(s3.page.index, 2);
        assert_eq!(a.page_count(), 3);
    }

    #[test]
    fn oversized_glyphs_get_their_own_page() {
        let mut a = atlas(2);
        let big = a.allocate(100, 40, 1).unwrap();
        assert_eq!(a.page_size_of(big.page), 128);
        // Regular glyphs still go on regular pages.
        assert!(a.allocate(5, 5, 1).is_some());
        assert!(a.allocate(MAX_PAGE_SIZE, 1, 1).is_none());
    }

    #[test]
    fn surplus_pages_are_trimmed_once_released() {
        let mut a = atlas(1);
        let s0 = a.allocate(30, 30, 1).unwrap();
        let lease = a.touch(s0.page, 1);
        let s1 = a.allocate(30, 30, 2).unwrap();
        assert_eq!((s1.page.index, a.page_count()), (1, 2));
        a.trim(3);
        assert_eq!(a.page_count(), 1, "the surplus page is unleased");
        drop(lease);
        // A recreated page never reuses a generation.
        let s2 = a.allocate(30, 30, 4).unwrap();
        assert_ne!(s2.page, s1.page);
        assert_ne!(s2.page.generation, s1.page.generation);
    }
}
