//! The palette schema: Material 3 system colour roles under Strand's
//! names, 1:1 (design.md, "Token model: three tiers"; the name table is
//! `docs/decisions.md`, wave2-check round 2), then the 13 hue roles
//! (design.md, "Hue roles"; decisions.md m5-tools), which have no
//! Material 3 name.

macro_rules! roles {
    ($($variant:ident => $name:literal, $m3:expr;)*) => {
        /// A palette role. `$accent` is Material 3's `primary`, `$fg` its
        /// `on_surface`, `$bg` its `background`; the rest keep M3's names
        /// or shorten them the same way.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Role {
            $($variant,)*
        }

        impl Role {
            /// Every role, in schema order.
            pub const ALL: &'static [Role] = &[$(Role::$variant,)*];

            /// How many roles a palette holds.
            pub const COUNT: usize = Role::ALL.len();

            /// The token name (`accent`, `on_accent`, `fg`).
            pub const fn name(self) -> &'static str {
                match self {
                    $(Role::$variant => $name,)*
                }
            }

            /// The Material 3 system role name (`primary`, `on_primary`,
            /// `on_surface`), as matugen and Material Theme Builder write it;
            /// `None` for a hue role.
            pub const fn m3(self) -> Option<&'static str> {
                match self {
                    $(Role::$variant => $m3,)*
                }
            }
        }
    };
}

roles! {
    Accent => "accent", Some("primary");
    OnAccent => "on_accent", Some("on_primary");
    AccentContainer => "accent_container", Some("primary_container");
    OnAccentContainer => "on_accent_container", Some("on_primary_container");
    Secondary => "secondary", Some("secondary");
    OnSecondary => "on_secondary", Some("on_secondary");
    SecondaryContainer => "secondary_container", Some("secondary_container");
    OnSecondaryContainer => "on_secondary_container", Some("on_secondary_container");
    Tertiary => "tertiary", Some("tertiary");
    OnTertiary => "on_tertiary", Some("on_tertiary");
    TertiaryContainer => "tertiary_container", Some("tertiary_container");
    OnTertiaryContainer => "on_tertiary_container", Some("on_tertiary_container");
    Error => "error", Some("error");
    OnError => "on_error", Some("on_error");
    ErrorContainer => "error_container", Some("error_container");
    OnErrorContainer => "on_error_container", Some("on_error_container");
    Bg => "bg", Some("background");
    OnBg => "on_bg", Some("on_background");
    Surface => "surface", Some("surface");
    Fg => "fg", Some("on_surface");
    SurfaceVariant => "surface_variant", Some("surface_variant");
    FgVariant => "fg_variant", Some("on_surface_variant");
    SurfaceDim => "surface_dim", Some("surface_dim");
    SurfaceBright => "surface_bright", Some("surface_bright");
    SurfaceLowest => "surface_lowest", Some("surface_container_lowest");
    SurfaceLow => "surface_low", Some("surface_container_low");
    SurfaceContainer => "surface_container", Some("surface_container");
    SurfaceHigh => "surface_high", Some("surface_container_high");
    SurfaceHighest => "surface_highest", Some("surface_container_highest");
    InverseSurface => "inverse_surface", Some("inverse_surface");
    InverseFg => "inverse_fg", Some("inverse_on_surface");
    InverseAccent => "inverse_accent", Some("inverse_primary");
    Outline => "outline", Some("outline");
    OutlineVariant => "outline_variant", Some("outline_variant");
    Shadow => "shadow", Some("shadow");
    Scrim => "scrim", Some("scrim");
    SurfaceTint => "surface_tint", Some("surface_tint");
    AccentFixed => "accent_fixed", Some("primary_fixed");
    AccentFixedDim => "accent_fixed_dim", Some("primary_fixed_dim");
    OnAccentFixed => "on_accent_fixed", Some("on_primary_fixed");
    OnAccentFixedVariant => "on_accent_fixed_variant", Some("on_primary_fixed_variant");
    SecondaryFixed => "secondary_fixed", Some("secondary_fixed");
    SecondaryFixedDim => "secondary_fixed_dim", Some("secondary_fixed_dim");
    OnSecondaryFixed => "on_secondary_fixed", Some("on_secondary_fixed");
    OnSecondaryFixedVariant => "on_secondary_fixed_variant", Some("on_secondary_fixed_variant");
    TertiaryFixed => "tertiary_fixed", Some("tertiary_fixed");
    TertiaryFixedDim => "tertiary_fixed_dim", Some("tertiary_fixed_dim");
    OnTertiaryFixed => "on_tertiary_fixed", Some("on_tertiary_fixed");
    OnTertiaryFixedVariant => "on_tertiary_fixed_variant", Some("on_tertiary_fixed_variant");
    Red => "red", None;
    Orange => "orange", None;
    Yellow => "yellow", None;
    Green => "green", None;
    Cyan => "cyan", None;
    Blue => "blue", None;
    Magenta => "magenta", None;
    BrightRed => "bright_red", None;
    BrightYellow => "bright_yellow", None;
    BrightGreen => "bright_green", None;
    BrightCyan => "bright_cyan", None;
    BrightBlue => "bright_blue", None;
    BrightMagenta => "bright_magenta", None;
}

impl Role {
    /// The 13 hue roles, after the Material 3 ones.
    pub const HUES: &'static [Role] = Role::ALL.split_at(Role::Red as usize).1;

    /// The role named `name`, by its Strand name or its Material 3 name.
    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL
            .iter()
            .copied()
            .find(|r| r.name() == name)
            .or_else(|| Role::ALL.iter().copied().find(|r| r.m3() == Some(name)))
    }

    /// Whether this is one of the 13 hue roles.
    pub fn is_hue(self) -> bool {
        self as usize >= Role::Red as usize
    }

    /// The role's index in [`Role::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_are_unique() {
        for (i, r) in Role::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
            assert_eq!(Role::from_name(r.name()), Some(*r));
            if let Some(m3) = r.m3() {
                assert_eq!(Role::from_name(m3), Some(*r));
            }
            assert_eq!(r.m3().is_none(), r.is_hue(), "{r}");
        }
        let mut names: Vec<_> = Role::ALL.iter().map(|r| r.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Role::COUNT);
        assert_eq!(Role::COUNT, 62);
        // strand-scene names the hue roots the same, in the same order.
        let hues: Vec<_> = Role::HUES.iter().map(|r| r.name()).collect();
        assert_eq!(hues, strand_scene::HUE_ROOTS);
        assert_eq!(Role::from_name("on_surface"), Some(Role::Fg));
    }
}
