//! Compositor capabilities: which optional protocols the compositor
//! offers, as the [`CompositorCaps`] the manager reports through
//! `SurfaceHost::compositor_caps` (docs/architecture.md, "strand-surface",
//! "M4 additions").
//!
//! The manager binds `wp_alpha_modifier_v1`, `wp_single_pixel_buffer_v1`
//! and `ext_background_effect_manager_v1` itself; the session lock and
//! the data device are only looked up here, for the streams that bind
//! them. Pure, so it is tested without a compositor.

use strand_scene::CompositorCaps;

/// The interface names this module looks for.
pub const ALPHA_MODIFIER: &str = "wp_alpha_modifier_v1";
pub const VIEWPORTER: &str = "wp_viewporter";
pub const SINGLE_PIXEL_BUFFER: &str = "wp_single_pixel_buffer_manager_v1";
pub const BACKGROUND_EFFECT: &str = "ext_background_effect_manager_v1";
pub const SESSION_LOCK: &str = "ext_session_lock_manager_v1";
pub const DATA_DEVICE: &str = "wl_data_device_manager";
/// The prefix of Hyprland's own protocols' globals
/// (`hyprland_surface_manager_v1`, `hyprland_focus_grab_manager_v1`, …).
pub const HYPRLAND_PREFIX: &str = "hyprland_";

/// `ext_background_effect_manager_v1.capability.blur`.
pub const BLUR: u32 = 1;

/// What the manager knows about the compositor's protocols.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Offered {
    /// The alpha modifier was bound.
    pub alpha_modifier: bool,
    /// The viewporter was bound.
    pub viewporter: bool,
    /// The single-pixel buffer manager was bound.
    pub single_pixel_buffer: bool,
    /// The background effect manager was bound.
    pub background_effect: bool,
    /// Its last `capabilities` flags (none until they arrive).
    pub effect_flags: u32,
    /// `ext_session_lock_manager_v1` is in the registry.
    pub session_lock: bool,
    /// `wl_data_device_manager` is in the registry.
    pub data_device: bool,
    /// A `hyprland_*` global is in the registry.
    pub hyprland: bool,
}

impl Offered {
    /// Marks the registry's interfaces this module only looks up.
    pub fn from_registry<'a>(interfaces: impl IntoIterator<Item = &'a str>) -> Self {
        let mut o = Offered::default();
        for i in interfaces {
            match i {
                SESSION_LOCK => o.session_lock = true,
                DATA_DEVICE => o.data_device = true,
                i if i.starts_with(HYPRLAND_PREFIX) => o.hyprland = true,
                _ => {}
            }
        }
        o
    }

    /// The capabilities to report. The background effect counts only
    /// once the compositor said it blurs (its `capabilities` event): a
    /// manager that cannot blur leaves the tint fallback in place.
    pub fn caps(&self) -> CompositorCaps {
        CompositorCaps {
            alpha_modifier: self.alpha_modifier,
            viewporter: self.viewporter,
            single_pixel_buffer: self.single_pixel_buffer,
            background_effect: self.background_effect && self.effect_flags & BLUR != 0,
            session_lock: self.session_lock,
            data_device: self.data_device,
            hyprland: self.hyprland,
        }
    }
}

/// Why `blur` falls back to its tint on this compositor (`None`: the
/// compositor blurs, through `ext-background-effect-v1`): [`blur_missing`]
/// with what `blur` draws instead.
pub fn blur_fallback_reason(caps: &CompositorCaps) -> Option<String> {
    let why = blur_missing(caps)?;
    let (head, hint) = why
        .split_once("; ")
        .map_or((why.as_str(), None), |(h, t)| (h, Some(t)));
    let tint =
        "so `blur` draws its tint fallback (alpha + 0.15; `blur_fallback: none` turns it off)";
    Some(match hint {
        Some(hint) => format!("{head}, {tint}; {hint}"),
        None => format!("{head}, {tint}"),
    })
}

/// Why no compositor blur is behind a `blur` box (`None`: the compositor
/// blurs). On Hyprland (`caps.hyprland`, from its globals) layer
/// surfaces blur through its own layer rules, which `strand
/// compositor-rules` prints, so the reason names that command.
pub fn blur_missing(caps: &CompositorCaps) -> Option<String> {
    if caps.background_effect {
        return None;
    }
    let why = "the compositor does not offer ext-background-effect-v1 with blur";
    Some(if caps.hyprland {
        format!(
            "{why}; on Hyprland, paste the layer rules `strand compositor-rules` prints \
             into your Hyprland config to blur Strand's surfaces"
        )
    } else {
        why.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_names_the_lock_and_the_data_device() {
        let o = Offered::from_registry(["wl_compositor", SESSION_LOCK, "wl_shm"]);
        assert!(o.session_lock && !o.data_device);
        let o = Offered::from_registry([DATA_DEVICE]);
        assert!(o.data_device && !o.session_lock);
        // Hyprland is known by its own globals, not by the environment
        // (a compositor nested in a Hyprland session inherits its
        // `HYPRLAND_INSTANCE_SIGNATURE`).
        let o = Offered::from_registry(["wl_compositor", "hyprland_focus_grab_manager_v1"]);
        assert!(o.hyprland && o.caps().hyprland);
        assert!(!Offered::from_registry(["zwlr_layer_shell_v1", "wl_seat"]).hyprland);
        assert_eq!(Offered::from_registry([]).caps(), CompositorCaps::default());
    }

    /// The background effect counts only with its blur capability.
    #[test]
    fn blur_needs_the_capability() {
        let mut o = Offered {
            background_effect: true,
            ..Offered::default()
        };
        assert!(!o.caps().background_effect, "no capabilities yet");
        o.effect_flags = BLUR;
        assert!(o.caps().background_effect);
        o.background_effect = false;
        assert!(!o.caps().background_effect, "flags without the manager");
        let o = Offered {
            alpha_modifier: true,
            viewporter: true,
            single_pixel_buffer: true,
            ..Offered::default()
        };
        let c = o.caps();
        assert!(c.delegates_poses() && c.single_pixel_buffer);
    }

    #[test]
    fn the_fallback_reason_names_the_rules_on_hyprland() {
        let blurs = CompositorCaps {
            background_effect: true,
            hyprland: true,
            ..CompositorCaps::default()
        };
        assert_eq!(blur_fallback_reason(&blurs), None);
        let none = CompositorCaps::default();
        let plain = blur_fallback_reason(&none).unwrap();
        assert!(plain.contains("ext-background-effect-v1") && plain.contains("tint"));
        assert!(!plain.contains("compositor-rules"));
        assert_eq!(
            plain,
            "the compositor does not offer ext-background-effect-v1 with blur, so `blur` draws \
             its tint fallback (alpha + 0.15; `blur_fallback: none` turns it off)"
        );
        assert_eq!(
            blur_missing(&none).unwrap(),
            "the compositor does not offer ext-background-effect-v1 with blur"
        );
        let hypr = blur_fallback_reason(&CompositorCaps {
            hyprland: true,
            ..none
        })
        .unwrap();
        assert!(hypr.contains("strand compositor-rules"));
        assert!(hypr.contains("tint fallback (alpha + 0.15"), "{hypr}");
    }
}
