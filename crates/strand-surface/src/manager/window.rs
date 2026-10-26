//! (window) `window` surfaces: an `xdg_toplevel` the compositor manages
//! (decisions.md `## window`). One per open `window` spec, wherever the
//! compositor puts it; its size is the compositor's where a configure
//! gives one it must or chose to keep (a tiling layout, a maximize, a
//! drag of its edge) and the spec's otherwise; its close is told to the
//! host as a click away, which writes its `open: <-> x` false; the
//! actions of its config (`w.maximize()` …) are asked here.

use std::num::NonZeroU32;

use smithay_client_toolkit::reexports::csd_frame::{WindowManagerCapabilities, WindowState};
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;

use super::*;
use crate::placement::{WindowConfig, window_config};

/// (window) What the compositor's last configure said of a window, as
/// [`SurfaceHost::window_state`] reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToplevelState {
    /// The axes on which the window takes the compositor's size rather
    /// than its own: one a state constrains (maximized, fullscreen,
    /// tiled, being resized) or that the compositor gave other than the
    /// window asked for and than any size of its own it committed since
    /// the last configure (a floating window the user resized). On the
    /// others the compositor left the size to it, or echoed it.
    pub sized: (bool, bool),
    /// Its box (window geometry) as configured, logical pixels.
    pub size: (u32, u32),
    pub maximized: bool,
    pub fullscreen: bool,
    /// Tiled on any edge.
    pub tiled: bool,
    /// The compositor focuses it (`xdg_toplevel` `activated`).
    pub activated: bool,
    /// The compositor draws its title bar and borders
    /// (`zxdg_toplevel_decoration_v1` server-side).
    pub decorated: bool,
    /// `xdg_toplevel.configure_bounds`: the largest box that fits the
    /// output it is on, logical pixels (`None`: not said).
    pub bounds: Option<(u32, u32)>,
    /// `xdg_toplevel.wm_capabilities`: the compositor shows a window
    /// menu, maximizes, fullscreens, minimizes (all, unless it said).
    pub window_menu: bool,
    pub can_maximize: bool,
    pub can_fullscreen: bool,
    pub can_minimize: bool,
}

/// A window surface's role state.
pub(super) struct Toplevel {
    pub(super) window: Window,
    pub(super) config: WindowConfig,
    pub(super) state: ToplevelState,
    /// The configure told the host last (`None`: none yet).
    pub(super) told: Option<ToplevelState>,
    /// On each axis, the sizes of its own (on an axis no configure
    /// sized) it committed since the compositor's last configure was
    /// acked, and the one it had then: a configure the compositor sent
    /// before it saw the latest commit gives one of these back, an
    /// echo, not the user's resize (`configure`).
    pub(super) echoes: [Vec<u32>; 2],
    /// The size of its own it last committed on each axis (`None`: that
    /// axis was the compositor's then).
    pub(super) own: [Option<u32>; 2],
}

/// At most this many sizes are kept per axis as echoes; a window does
/// not commit more own sizes than this between two configures it reads.
const ECHOES: usize = 8;

impl Toplevel {
    /// The box `size` goes out with the next commit (its window
    /// geometry): on an axis it sized itself, a size a configure may
    /// still echo.
    pub(super) fn committing(&mut self, size: (u32, u32)) {
        let sized = [self.state.sized.0, self.state.sized.1];
        for (i, v) in [size.0, size.1].into_iter().enumerate() {
            if sized[i] {
                self.own[i] = None;
                continue;
            }
            self.own[i] = Some(v);
            let e = &mut self.echoes[i];
            if !e.contains(&v) {
                if e.len() == ECHOES {
                    e.remove(0);
                }
                e.push(v);
            }
        }
    }
}

/// The decorations to ask for at creation: the compositor's (it may
/// refuse, `w.decorated` false then) or none (the config draws them).
fn decorations(d: strand_scene::Decorations) -> WindowDecorations {
    match d {
        strand_scene::Decorations::Server => WindowDecorations::RequestServer,
        strand_scene::Decorations::None => WindowDecorations::RequestClient,
    }
}

fn opt_size((w, h): (u32, u32)) -> Option<(u32, u32)> {
    (w > 0 || h > 0).then_some((w, h))
}

/// The size a window keeps on one axis from a configure that gives it
/// `given` (`None`: 0, its choice): [`crate::placement::window_kept_size`],
/// shared with the evaluation sandbox's compositor.
fn kept_size(given: Option<NonZeroU32>, asked: u32, constrained: bool) -> (bool, u32) {
    crate::placement::window_kept_size(given.map(NonZeroU32::get), asked, constrained)
}

/// The `xdg_toplevel` edge a `w.resize(edge)` names (`center`: none).
fn resize_edge(a: strand_scene::Anchor) -> Option<ResizeEdge> {
    use strand_scene::Anchor;
    Some(match a {
        Anchor::Top => ResizeEdge::Top,
        Anchor::Bottom => ResizeEdge::Bottom,
        Anchor::Left => ResizeEdge::Left,
        Anchor::Right => ResizeEdge::Right,
        Anchor::TopLeft => ResizeEdge::TopLeft,
        Anchor::TopRight => ResizeEdge::TopRight,
        Anchor::BottomLeft => ResizeEdge::BottomLeft,
        Anchor::BottomRight => ResizeEdge::BottomRight,
        Anchor::Center => return None,
    })
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Creates or destroys `node`'s window so there is one exactly while
    /// its spec is open (and render has sized it).
    pub(super) fn reconcile_window(&mut self, node: NodeId, spec: &SurfaceSpec) {
        let existing = self.surfaces_of(node);
        let config = spec.open.then(|| window_config(spec).ok()).flatten();
        if !spec.open {
            self.window_asks.remove(&node);
        }
        let Some(config) = config.filter(|_| self.xdg_shell.is_some()) else {
            if spec.open && self.xdg_shell.is_none() {
                log::warn!(
                    "{}: no xdg_wm_base, windows are not shown",
                    spec.namespace()
                );
            }
            for id in existing {
                self.destroy_surface(id);
            }
            return;
        };
        if existing.is_empty() {
            self.create_window(node, spec, config);
        }
    }

    fn create_window(&mut self, node: NodeId, spec: &SurfaceSpec, config: WindowConfig) {
        let Some(shell) = self.xdg_shell.as_ref() else {
            return;
        };
        let wl = self.compositor.create_surface(&self.qh);
        let window = shell.create_window(wl.clone(), decorations(config.decorations), &self.qh);
        window.set_title(config.title.clone());
        window.set_app_id(config.app_id.clone());
        window.set_min_size(opt_size(config.min));
        window.set_max_size(opt_size(config.max));
        // (window) What the handler that opened it asked (`w.maximize()`
        // in the step that set `shown = true`): asked before the first
        // commit, so its first configure has it.
        let (mut max, mut full, mut min) = (false, false, false);
        for ask in self.window_asks.remove(&node).unwrap_or_default() {
            match ask {
                strand_scene::WindowAsk::Maximize => max = !max,
                strand_scene::WindowAsk::Fullscreen => full = !full,
                strand_scene::WindowAsk::Minimize => min = true,
                _ => {}
            }
        }
        if max {
            window.set_maximized();
        }
        if full {
            window.set_fullscreen(None);
        }
        if min {
            window.set_minimized();
        }
        let key = (node, Placement::Focused);
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(id)));
        let fractional = match (&viewport, &self.fractional_manager) {
            (Some(_), Some(fm)) => Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            _ => None,
        };
        // Wherever the compositor puts it: a scale every output shares
        // until it says.
        let (scale, integer_scale) = self.initial_scale(None, fractional.is_some());
        // The initial commit, with no buffer: the compositor answers with
        // the first configure.
        wl.commit();
        self.by_wl.insert(wl.id(), id);
        log::debug!(
            "{}: window {:?} ({}) asking {}×{}",
            config.namespace,
            config.title,
            config.app_id,
            config.width,
            config.height
        );
        let layer_like = config.as_layer();
        let surface = Surface {
            id,
            generation,
            node,
            kind: spec.kind,
            placement: Placement::Focused,
            monitor: None,
            output: None,
            requested_output: None,
            role: Role::Window(Box::new(Toplevel {
                window,
                config,
                state: ToplevelState::default(),
                told: None,
                echoes: [Vec::new(), Vec::new()],
                own: [None, None],
            })),
            config: layer_like,
            viewport,
            fractional,
            configured: false,
            logical: (0, 0),
            scale,
            reported_scale: None,
            integer_scale,
            buffers: ShmBuffers::new(id, self.max_buffers),
            geometry_dirty: true,
            callback_pending: false,
            commit_seq: 0,
            in_flight: None,
            throttled_at: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            blur: None,
            blur_sent: Some(Vec::new()),
            pose: strand_scene::SurfacePose::IDENTITY,
            alpha: None,
            origin: None,
            zone_on: None,
            last_damage: Vec::new(),
            click_through: false,
            input_region: None,
            painted_input: None,
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        self.sync_idle_inhibitor(id);
        self.host.surface_attached(id, node, None);
    }

    /// A window spec changed: its title, app id, size limits and
    /// decorations are told in place; a new size it asks for is taken on
    /// the axes the compositor left to it.
    pub(super) fn reconfigure_window(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        let Ok(new) = window_config(&spec) else {
            return;
        };
        for id in self.surfaces_of(node) {
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let Role::Window(t) = &mut s.role else {
                continue;
            };
            let old = std::mem::replace(&mut t.config, new.clone());
            if old == new {
                continue;
            }
            if old.title != new.title {
                t.window.set_title(new.title.clone());
            }
            if old.app_id != new.app_id {
                t.window.set_app_id(new.app_id.clone());
            }
            if old.min != new.min {
                t.window.set_min_size(opt_size(new.min));
            }
            if old.max != new.max {
                t.window.set_max_size(opt_size(new.max));
            }
            if old.decorations != new.decorations {
                t.window
                    .request_decoration_mode(Some(match new.decorations {
                        strand_scene::Decorations::Server => DecorationMode::Server,
                        strand_scene::Decorations::None => DecorationMode::Client,
                    }));
            }
            s.config = new.as_layer();
            if !s.configured {
                // Its first configure takes the new size.
                continue;
            }
            // Its own size on the axes the compositor left to it.
            let (sw, sh) = t.state.sized;
            let (cw, ch) = t.state.size;
            let size = (
                if sw { cw } else { new.width },
                if sh { ch } else { new.height },
            );
            let buffer = new.buffer(size);
            if buffer != s.logical {
                log::debug!(
                    "{}: window resized to {}×{} (box {}×{})",
                    new.namespace,
                    buffer.0,
                    buffer.1,
                    size.0,
                    size.1
                );
                t.state.size = size;
                s.logical = buffer;
                s.repaint = true;
            }
            // The window geometry (and its overhang) with the next commit.
            s.geometry_dirty = true;
            self.sync_input_region(id);
            self.mark(id);
        }
    }

    /// (window) Asks the compositor what `w.maximize()` and the other
    /// window actions of `node`'s config ask (`SceneDiff::window_asks`):
    /// `press` is the serial of the press whose handler asked, for a
    /// move or resize (the compositor checks it is a held button's);
    /// without one the seat's last. `w.close()` closes it as the
    /// compositor's close does (a click away for the host). An action
    /// the compositor does not offer (`wm_capabilities`) does nothing.
    /// A maximize, fullscreen or minimize asked of a window its spec
    /// opens but that is not made yet (render has not sized it) is asked
    /// when it is made; a move, resize or close of one not shown does
    /// nothing.
    pub fn window_ask(&mut self, node: NodeId, ask: strand_scene::WindowAsk, press: Option<u32>) {
        use strand_scene::WindowAsk;
        let Some(id) = self.surfaces_of(node).into_iter().find(|id| {
            matches!(
                self.surfaces.get(id).map(|s| &s.role),
                Some(Role::Window(_))
            )
        }) else {
            let opening = self
                .specs
                .get(&node)
                .is_some_and(|s| s.kind == NodeKind::Window && s.open);
            if opening
                && matches!(
                    ask,
                    WindowAsk::Maximize | WindowAsk::Fullscreen | WindowAsk::Minimize
                )
            {
                log::debug!("{node:?}: {ask:?} asked when its window is made");
                self.window_asks.entry(node).or_default().push(ask);
            } else {
                log::debug!("{node:?}: {ask:?} asked of no window shown");
            }
            return;
        };
        if ask == WindowAsk::Close {
            self.send_input(InputEvent::ClickAway { surface: id });
            return;
        }
        let seat = self
            .last_action
            .as_ref()
            .map(|a| (a.seat.clone(), a.serial));
        let Some(Role::Window(t)) = self.surfaces.get(&id).map(|s| &s.role) else {
            return;
        };
        let st = t.state;
        let w = &t.window;
        let mut handed = false;
        match ask {
            WindowAsk::Move | WindowAsk::Resize(_) => {
                let Some((seat, last)) = seat else {
                    log::debug!("{node:?}: {ask:?} with no press to start it");
                    return;
                };
                let serial = press.unwrap_or(last);
                match ask {
                    WindowAsk::Resize(a) => match resize_edge(a) {
                        Some(edge) => {
                            w.resize(&seat, serial, edge);
                            handed = true;
                        }
                        None => log::debug!("{node:?}: a resize from the centre does nothing"),
                    },
                    _ => {
                        w.move_(&seat, serial);
                        handed = true;
                    }
                }
            }
            WindowAsk::Minimize if st.can_minimize => w.set_minimized(),
            WindowAsk::Maximize if st.can_maximize => {
                if st.maximized {
                    w.unset_maximized()
                } else {
                    w.set_maximized()
                }
            }
            WindowAsk::Fullscreen if st.can_fullscreen => {
                if st.fullscreen {
                    w.unset_fullscreen()
                } else {
                    w.set_fullscreen(None)
                }
            }
            other => log::debug!("{node:?}: the compositor does not offer {other:?}"),
        }
        // The press now drives the compositor's move or resize, so for
        // strand it ends here, as on a leave: sway, niri and labwc send a
        // leave at once, but Hyprland sends the button's release when the
        // move ends, on the spot of the window the press was on (it moved
        // with the pointer), which would end the press there as a click.
        if handed {
            log::debug!("{node:?}: {ask:?} handed to the compositor: the press ends here");
            self.send_input(InputEvent::PointerLeave { surface: id });
        }
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the Wayland connection failed: {e}");
        }
    }

    /// (window) What the compositor last said of the window showing
    /// `node` (tests, `strand report`).
    pub fn toplevel_state(&self, node: NodeId) -> Option<ToplevelState> {
        self.surfaces.values().find_map(|s| match &s.role {
            Role::Window(t) if s.node == node && t.told.is_some() => Some(t.state),
            _ => None,
        })
    }
}

impl<H: SurfaceHost + 'static> WindowHandler for State<H> {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        // Its `open: <-> x` goes false through the router (a click away
        // for the host); the spec closing destroys it. With no two-way
        // `open`, nothing closes it: xdg-shell lets a client stay.
        let Some(id) = self.surface_for(window.wl_surface()) else {
            return;
        };
        log::debug!("{id:?}: the compositor closes the window");
        self.stats.closed += 1;
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.stats.closed += 1;
        }
        self.host.window_closed(id);
        self.send_input(InputEvent::ClickAway { surface: id });
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(id) = self.surface_for(window.wl_surface()) else {
            return;
        };
        let Some(Role::Window(t)) = self.surfaces.get_mut(&id).map(|s| &mut s.role) else {
            return;
        };
        let st = configure.state;
        let constrained = st.intersects(
            WindowState::MAXIMIZED
                | WindowState::FULLSCREEN
                | WindowState::TILED
                | WindowState::RESIZING,
        );
        let c = t.config.clone();
        // A size it committed of its own since the last configure was
        // acked is an echo of it, from a configure sent before the
        // compositor saw the latest commit (sway sends one on every
        // focus change): its own size stays, not taken as the user's
        // resize, which would stop its content sizing it.
        let echoes = std::mem::replace(&mut t.echoes, t.own.map(|o| o.into_iter().collect()));
        let axis = |i: usize, given: Option<NonZeroU32>, asked: u32| match given {
            Some(g) if !constrained && echoes[i].contains(&g.get()) => (false, asked.max(1)),
            _ => kept_size(given, asked, constrained),
        };
        let (sw, w) = axis(0, configure.new_size.0, c.width);
        let (sh, h) = axis(1, configure.new_size.1, c.height);
        let caps = configure.capabilities;
        let state = ToplevelState {
            sized: (sw, sh),
            size: (w, h),
            maximized: st.contains(WindowState::MAXIMIZED),
            fullscreen: st.contains(WindowState::FULLSCREEN),
            tiled: st.intersects(WindowState::TILED),
            activated: st.contains(WindowState::ACTIVATED),
            decorated: configure.decoration_mode == DecorationMode::Server,
            bounds: configure.suggested_bounds,
            window_menu: caps.contains(WindowManagerCapabilities::WINDOW_MENU),
            can_maximize: caps.contains(WindowManagerCapabilities::MAXIMIZE),
            can_fullscreen: caps.contains(WindowManagerCapabilities::FULLSCREEN),
            can_minimize: caps.contains(WindowManagerCapabilities::MINIMIZE),
        };
        let buffer = c.buffer((w, h));
        log::debug!(
            "{}: window configured {:?}×{:?} ({}×{} kept), {:?}, decoration {:?}",
            c.namespace,
            configure.new_size.0,
            configure.new_size.1,
            w,
            h,
            st,
            configure.decoration_mode
        );
        t.state = state;
        let tell = t.told != Some(state);
        t.told = Some(state);
        // The host hears what changed before the size: render lays a
        // compositor-sized window out at it, holding no frame for another.
        if tell {
            self.host.window_state(id, &state);
        }
        self.configured(id, buffer);
        // sctk acked it already; the window geometry rides the next
        // commit.
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.geometry_dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(v)
    }

    #[test]
    fn the_compositors_size_is_kept_where_it_decides() {
        // 0: the window's choice, what it asked for.
        assert_eq!(kept_size(None, 400, false), (false, 400));
        // An echo of what it asked for leaves the size to it, so its
        // content can still grow it.
        assert_eq!(kept_size(n(400), 400, false), (false, 400));
        // Another size, floating: the user resized it.
        assert_eq!(kept_size(n(520), 400, false), (true, 520));
        // Tiled, maximized, fullscreen or resizing: the compositor's,
        // even when it happens to be the asked size.
        assert_eq!(kept_size(n(400), 400, true), (true, 400));
        assert_eq!(kept_size(n(960), 400, true), (true, 960));
        // Constrained with no size given: its own.
        assert_eq!(kept_size(None, 400, true), (false, 400));
        assert_eq!(kept_size(None, 0, false), (false, 1));
    }

    #[test]
    fn resize_edges_map_to_xdg_toplevel_edges() {
        use strand_scene::Anchor;
        assert_eq!(
            resize_edge(Anchor::BottomRight),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(resize_edge(Anchor::Top), Some(ResizeEdge::Top));
        assert_eq!(resize_edge(Anchor::Left), Some(ResizeEdge::Left));
        assert_eq!(resize_edge(Anchor::Center), None);
    }

    #[test]
    fn decorations_ask_the_compositor_or_not() {
        assert_eq!(
            decorations(strand_scene::Decorations::Server),
            WindowDecorations::RequestServer
        );
        assert_eq!(
            decorations(strand_scene::Decorations::None),
            WindowDecorations::RequestClient
        );
        assert_eq!(opt_size((0, 0)), None);
        assert_eq!(opt_size((200, 0)), Some((200, 0)));
    }
}
