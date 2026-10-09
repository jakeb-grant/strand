//! The surface side of the fake: `wl_compositor`, `wl_region`, `wl_shm`,
//! `zwlr_layer_shell_v1`, `wp_viewporter`, `wp_single_pixel_buffer_v1`,
//! `wp_alpha_modifier_v1` and `ext_background_effect_v1`, enough for the
//! surface manager (`strand-surface`) to map layer surfaces, and recording
//! what each surface committed so tests can check the protocol state the
//! compositor saw (sway 1.9 in CI offers neither the alpha modifier nor
//! the background effect).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wayland_protocols::ext::background_effect::v1::server::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};
use wayland_protocols::wp::alpha_modifier::v1::server::{
    wp_alpha_modifier_surface_v1::{self, WpAlphaModifierSurfaceV1},
    wp_alpha_modifier_v1::{self, WpAlphaModifierV1},
};
use wayland_protocols::wp::single_pixel_buffer::v1::server::wp_single_pixel_buffer_manager_v1::{
    self, WpSinglePixelBufferManagerV1,
};
use wayland_protocols::wp::viewporter::server::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wayland_server::backend::ObjectId;
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::Server;

/// Which surface globals the fake offers (all but the core ones are
/// optional, as on real compositors).
#[derive(Clone, Debug)]
pub struct SurfaceGlobals {
    /// `wp_viewporter`.
    pub viewporter: bool,
    /// `wp_single_pixel_buffer_manager_v1`.
    pub single_pixel_buffer: bool,
    /// `wp_alpha_modifier_v1`.
    pub alpha_modifier: bool,
    /// `ext_background_effect_manager_v1`, with the capability flags it
    /// sends on bind (1: blur); `None` does not offer it.
    pub background_effect: Option<u32>,
    /// Each output's size in pixels (scale 1), by the order of `outputs`.
    pub output_size: (u32, u32),
}

impl Default for SurfaceGlobals {
    /// Every protocol, with blur.
    fn default() -> Self {
        SurfaceGlobals {
            viewporter: true,
            single_pixel_buffer: true,
            alpha_modifier: true,
            background_effect: Some(1),
            output_size: (1920, 1080),
        }
    }
}

/// One step of a `wl_region`: `add` (true) or `subtract` a rectangle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RegionOp {
    pub add: bool,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A region as the client built it (`add`/`subtract` in order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region(pub Vec<RegionOp>);

impl Region {
    /// True if the pixel at `(x, y)` (its top-left corner) is inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        let mut inside = false;
        for op in &self.0 {
            if x >= op.x && y >= op.y && x < op.x + op.w && y < op.y + op.h {
                inside = op.add;
            }
        }
        inside
    }

    /// Pixels inside, counted over the bounds of its `add`s.
    pub fn area(&self) -> u64 {
        let adds = self.0.iter().filter(|o| o.add);
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for o in adds {
            x0 = x0.min(o.x);
            y0 = y0.min(o.y);
            x1 = x1.max(o.x + o.w);
            y1 = y1.max(o.y + o.h);
        }
        let mut n = 0;
        for y in y0..y1.max(y0) {
            for x in x0..x1.max(x0) {
                n += u64::from(self.contains(x, y));
            }
        }
        n
    }
}

/// What a committed buffer was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BufferKind {
    /// A `wl_shm` buffer of this size.
    Shm { width: i32, height: i32 },
    /// A `wp_single_pixel_buffer_v1` buffer: red, green, blue and alpha,
    /// premultiplied, each of `u32::MAX`.
    SinglePixel([u32; 4]),
}

/// One surface as the fake last saw it committed.
#[derive(Clone, Debug, Default)]
pub struct SurfaceRecord {
    /// The layer surface's namespace (`None`: not a layer surface).
    pub namespace: Option<String>,
    /// Its layer (`zwlr_layer_shell_v1.layer`).
    pub layer: Option<u32>,
    /// The size it was last configured at.
    pub configured: Option<(u32, u32)>,
    /// The buffer of its last commit that attached one (`None`: none
    /// yet, or detached).
    pub buffer: Option<BufferKind>,
    /// The viewport's destination, as of the last commit.
    pub viewport: Option<(i32, i32)>,
    /// The input region as of the last commit (`None`: the whole
    /// surface).
    pub input: Option<Region>,
    /// The blur region as of the last commit (`None`: no blur).
    pub blur: Option<Region>,
    /// Each commit that carried a `set_blur_region`, with what it set
    /// (`None`: the effect removed).
    pub blur_sets: Vec<Option<Region>>,
    /// The alpha multiplier as of the last commit.
    pub alpha: Option<u32>,
    /// Commits so far, with and without a buffer.
    pub commits: usize,
    /// Commits that attached a buffer.
    pub buffer_commits: usize,
    /// The client destroyed it.
    pub destroyed: bool,
}

/// What a surface has pending until its next commit.
#[derive(Default)]
struct Pending {
    buffer: Option<Option<wl_buffer::WlBuffer>>,
    viewport: Option<Option<(i32, i32)>>,
    input: Option<Option<Region>>,
    blur: Option<Option<Region>>,
    alpha: Option<u32>,
    frames: Vec<wl_callback::WlCallback>,
}

/// A layer surface's requested state.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct LayerRequest {
    size: (u32, u32),
    anchor: u32,
    margin: [i32; 4],
}

struct Live {
    /// Index into the records (creation order).
    index: usize,
    pending: Pending,
    /// The buffer on screen, released when another replaces it.
    current: Option<wl_buffer::WlBuffer>,
    layer: Option<ZwlrLayerSurfaceV1>,
    layer_request: LayerRequest,
    /// The request last configured (`None`: not configured yet).
    layer_configured: Option<LayerRequest>,
    serial: u32,
}

/// The surface state of the fake.
#[derive(Default)]
pub(crate) struct Surfaces {
    pub(crate) globals: Option<SurfaceGlobals>,
    live: HashMap<ObjectId, Live>,
    /// Shared with [`crate::Fake`]: every surface ever created.
    pub(crate) records: Arc<Mutex<Vec<SurfaceRecord>>>,
    effect_managers: Vec<ExtBackgroundEffectManagerV1>,
    /// The blur capability flags sent now.
    pub(crate) effect_caps: u32,
}

impl Surfaces {
    fn record(&self, surface: &ObjectId, f: impl FnOnce(&mut SurfaceRecord)) {
        let Some(live) = self.live.get(surface) else {
            return;
        };
        if let Ok(mut r) = self.records.lock()
            && let Some(rec) = r.get_mut(live.index)
        {
            f(rec);
        }
    }

    /// Sends new capability flags to every bound effect manager.
    pub(crate) fn set_effect_caps(&mut self, flags: u32) {
        self.effect_caps = flags;
        for m in &self.effect_managers {
            m.capabilities(ext_background_effect_manager_v1::Capability::from_bits_truncate(flags));
        }
    }

    fn output_size(&self) -> (u32, u32) {
        self.globals
            .as_ref()
            .map_or((1920, 1080), |g| g.output_size)
    }

    fn commit(&mut self, surface: &ObjectId) {
        let (w, h) = self.output_size();
        let Some(live) = self.live.get_mut(surface) else {
            return;
        };
        let pending = std::mem::take(&mut live.pending);
        let mut attached = None;
        if let Some(buffer) = pending.buffer {
            if let Some(old) = live.current.take()
                && Some(&old) != buffer.as_ref()
                && old.is_alive()
            {
                old.release();
            }
            attached = Some(
                buffer
                    .as_ref()
                    .and_then(|b| b.data::<BufferKind>().copied()),
            );
            live.current = buffer;
        }
        // A layer surface is configured on its first commit and whenever
        // what it asks for changed.
        let mut configure = None;
        if let Some(layer) = &live.layer
            && live.layer_configured != Some(live.layer_request)
        {
            let r = live.layer_request;
            let stretch = |size: u32, lo: bool, hi: bool, out: u32, m: i32, n: i32| {
                if size == 0 && lo && hi {
                    (out as i32 - m - n).max(1) as u32
                } else {
                    size
                }
            };
            let a = r.anchor;
            let (top, bottom, left, right) = (a & 1 != 0, a & 2 != 0, a & 4 != 0, a & 8 != 0);
            let cw = stretch(r.size.0, left, right, w, r.margin[3], r.margin[1]);
            let ch = stretch(r.size.1, top, bottom, h, r.margin[0], r.margin[2]);
            live.serial += 1;
            layer.configure(live.serial, cw, ch);
            live.layer_configured = Some(r);
            configure = Some((cw, ch));
        }
        let index = live.index;
        for cb in pending.frames {
            cb.done(0);
        }
        if let Ok(mut r) = self.records.lock()
            && let Some(rec) = r.get_mut(index)
        {
            rec.commits += 1;
            if let Some(b) = attached {
                rec.buffer = b;
                if b.is_some() {
                    rec.buffer_commits += 1;
                }
            }
            if let Some(v) = pending.viewport {
                rec.viewport = v;
            }
            if let Some(i) = pending.input {
                rec.input = i;
            }
            if let Some(b) = pending.blur {
                rec.blur = b.clone();
                rec.blur_sets.push(b);
            }
            if let Some(a) = pending.alpha {
                rec.alpha = Some(a);
            }
            if let Some(c) = configure {
                rec.configured = Some(c);
            }
        }
    }
}

fn region_of(r: Option<&wl_region::WlRegion>) -> Option<Region> {
    r.and_then(|r| r.data::<Mutex<Region>>())
        .and_then(|m| m.lock().ok().map(|g| g.clone()))
}

// ---- wl_compositor, wl_surface, wl_region, wl_callback ----------------------

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let s = init.init(id, ());
                let index = match state.surf.records.lock() {
                    Ok(mut r) => {
                        r.push(SurfaceRecord::default());
                        r.len() - 1
                    }
                    Err(_) => return,
                };
                state.surf.live.insert(
                    s.id(),
                    Live {
                        index,
                        pending: Pending::default(),
                        current: None,
                        layer: None,
                        layer_request: LayerRequest::default(),
                        layer_configured: None,
                        serial: 0,
                    },
                );
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, Mutex::new(Region::default()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let id = surface.id();
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                if let Some(l) = state.surf.live.get_mut(&id) {
                    l.pending.buffer = Some(buffer);
                }
            }
            wl_surface::Request::Frame { callback } => {
                let cb = init.init(callback, ());
                if let Some(l) = state.surf.live.get_mut(&id) {
                    l.pending.frames.push(cb);
                }
            }
            wl_surface::Request::SetInputRegion { region } => {
                if let Some(l) = state.surf.live.get_mut(&id) {
                    l.pending.input = Some(region_of(region.as_ref()));
                }
            }
            wl_surface::Request::Commit => state.surf.commit(&id),
            wl_surface::Request::Destroy => {
                state.surf.record(&id, |r| r.destroyed = true);
                state.surf.live.remove(&id);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        s: &wl_surface::WlSurface,
        _: &(),
    ) {
        let id = s.id();
        state.surf.record(&id, |r| r.destroyed = true);
        state.surf.live.remove(&id);
    }
}

impl Dispatch<wl_region::WlRegion, Mutex<Region>> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        request: wl_region::Request,
        data: &Mutex<Region>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let op = match request {
            wl_region::Request::Add {
                x,
                y,
                width,
                height,
            } => RegionOp {
                add: true,
                x,
                y,
                w: width,
                h: height,
            },
            wl_region::Request::Subtract {
                x,
                y,
                width,
                height,
            } => RegionOp {
                add: false,
                x,
                y,
                w: width,
                h: height,
            },
            _ => return,
        };
        if let Ok(mut r) = data.lock() {
            r.0.push(op);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_callback::WlCallback,
        _: wl_callback::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// ---- wl_shm -------------------------------------------------------------------

impl GlobalDispatch<wl_shm::WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            // The memory is never read: the fake records sizes only.
            init.init(id, ());
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer {
            id, width, height, ..
        } = request
        {
            init.init(id, BufferKind::Shm { width, height });
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferKind> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &BufferKind,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// ---- zwlr_layer_shell_v1 --------------------------------------------------------

impl GlobalDispatch<ZwlrLayerShellV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrLayerShellV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_shell_v1::Request::GetLayerSurface {
            id,
            surface,
            layer,
            namespace,
            ..
        } = request
        {
            let sid = surface.id();
            let l = init.init(id, sid.clone());
            let layer = layer.into_result().map(u32::from).ok();
            state.surf.record(&sid, |r| {
                r.namespace = Some(namespace);
                r.layer = layer;
            });
            if let Some(live) = state.surf.live.get_mut(&sid) {
                live.layer = Some(l);
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(live) = state.surf.live.get_mut(surface) else {
            return;
        };
        match request {
            zwlr_layer_surface_v1::Request::SetSize { width, height } => {
                live.layer_request.size = (width, height);
            }
            zwlr_layer_surface_v1::Request::SetAnchor { anchor } => {
                live.layer_request.anchor = anchor.into_result().map_or(0, |a| a.bits());
            }
            zwlr_layer_surface_v1::Request::SetMargin {
                top,
                right,
                bottom,
                left,
            } => live.layer_request.margin = [top, right, bottom, left],
            zwlr_layer_surface_v1::Request::Destroy => live.layer = None,
            _ => {}
        }
    }
}

// ---- wp_viewporter ------------------------------------------------------------------

impl GlobalDispatch<WpViewporter, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpViewporter>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<WpViewporter, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpViewporter,
        request: wp_viewporter::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wp_viewporter::Request::GetViewport { id, surface } = request {
            init.init(id, surface.id());
        }
    }
}

impl Dispatch<WpViewport, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpViewport,
        request: wp_viewport::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(live) = state.surf.live.get_mut(surface) else {
            return;
        };
        match request {
            wp_viewport::Request::SetDestination { width, height } => {
                live.pending.viewport = Some((width > 0).then_some((width, height)));
            }
            wp_viewport::Request::Destroy => live.pending.viewport = Some(None),
            _ => {}
        }
    }
}

// ---- wp_single_pixel_buffer_v1 ---------------------------------------------------------

impl GlobalDispatch<WpSinglePixelBufferManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpSinglePixelBufferManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpSinglePixelBufferManagerV1,
        request: wp_single_pixel_buffer_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wp_single_pixel_buffer_manager_v1::Request::CreateU32RgbaBuffer { id, r, g, b, a } =
            request
        {
            init.init(id, BufferKind::SinglePixel([r, g, b, a]));
        }
    }
}

// ---- wp_alpha_modifier_v1 ------------------------------------------------------------

impl GlobalDispatch<WpAlphaModifierV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpAlphaModifierV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<WpAlphaModifierV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpAlphaModifierV1,
        request: wp_alpha_modifier_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wp_alpha_modifier_v1::Request::GetSurface { id, surface } = request {
            init.init(id, surface.id());
        }
    }
}

impl Dispatch<WpAlphaModifierSurfaceV1, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpAlphaModifierSurfaceV1,
        request: wp_alpha_modifier_surface_v1::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wp_alpha_modifier_surface_v1::Request::SetMultiplier { factor } = request
            && let Some(live) = state.surf.live.get_mut(surface)
        {
            live.pending.alpha = Some(factor);
        }
    }
}

// ---- ext_background_effect_v1 ------------------------------------------------------------

impl GlobalDispatch<ExtBackgroundEffectManagerV1, ()> for Server {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtBackgroundEffectManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let m = init.init(resource, ());
        m.capabilities(
            ext_background_effect_manager_v1::Capability::from_bits_truncate(
                state.surf.effect_caps,
            ),
        );
        state.surf.effect_managers.push(m);
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &ExtBackgroundEffectManagerV1,
        request: ext_background_effect_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_background_effect_manager_v1::Request::GetBackgroundEffect { id, surface } => {
                init.init(id, surface.id());
            }
            ext_background_effect_manager_v1::Request::Destroy => {
                state.surf.effect_managers.retain(|m| m != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtBackgroundEffectSurfaceV1,
        request: ext_background_effect_surface_v1::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(live) = state.surf.live.get_mut(surface) else {
            return;
        };
        match request {
            ext_background_effect_surface_v1::Request::SetBlurRegion { region } => {
                live.pending.blur = Some(region_of(region.as_ref()));
            }
            // Its regions go with the next commit.
            ext_background_effect_surface_v1::Request::Destroy => live.pending.blur = Some(None),
            _ => {}
        }
    }
}

/// Creates the surface globals `g` asks for.
pub(crate) fn create_globals(dh: &DisplayHandle, g: &SurfaceGlobals) {
    dh.create_global::<Server, wl_compositor::WlCompositor, ()>(6, ());
    dh.create_global::<Server, wl_shm::WlShm, ()>(1, ());
    dh.create_global::<Server, ZwlrLayerShellV1, ()>(4, ());
    if g.viewporter {
        dh.create_global::<Server, WpViewporter, ()>(1, ());
    }
    if g.single_pixel_buffer {
        dh.create_global::<Server, WpSinglePixelBufferManagerV1, ()>(1, ());
    }
    if g.alpha_modifier {
        dh.create_global::<Server, WpAlphaModifierV1, ()>(1, ());
    }
    if g.background_effect.is_some() {
        dh.create_global::<Server, ExtBackgroundEffectManagerV1, ()>(1, ());
    }
}
