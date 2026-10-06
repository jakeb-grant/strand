//! The palette schema: Material 3 system colour roles under Strand's
//! names, 1:1 (design.md, "Token model: three tiers"; the name table is
//! `docs/decisions.md`, wave2-check round 2).

macro_rules! roles {
    ($($variant:ident => $name:literal, $m3:literal;)*) => {
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
            /// `on_surface`), as matugen and Material Theme Builder write it.
            pub const fn m3(self) -> &'static str {
                match self {
                    $(Role::$variant => $m3,)*
                }
            }
        }
    };
}

roles! {
    Accent => "accent", "primary";
    OnAccent => "on_accent", "on_primary";
    AccentContainer => "accent_container", "primary_container";
    OnAccentContainer => "on_accent_container", "on_primary_container";
    Secondary => "secondary", "secondary";
    OnSecondary => "on_secondary", "on_secondary";
    SecondaryContainer => "secondary_container", "secondary_container";
    OnSecondaryContainer => "on_secondary_container", "on_secondary_container";
    Tertiary => "tertiary", "tertiary";
    OnTertiary => "on_tertiary", "on_tertiary";
    TertiaryContainer => "tertiary_container", "tertiary_container";
    OnTertiaryContainer => "on_tertiary_container", "on_tertiary_container";
    Error => "error", "error";
    OnError => "on_error", "on_error";
    ErrorContainer => "error_container", "error_container";
    OnErrorContainer => "on_error_container", "on_error_container";
    Bg => "bg", "background";
    OnBg => "on_bg", "on_background";
    Surface => "surface", "surface";
    Fg => "fg", "on_surface";
    SurfaceVariant => "surface_variant", "surface_variant";
    FgVariant => "fg_variant", "on_surface_variant";
    SurfaceDim => "surface_dim", "surface_dim";
    SurfaceBright => "surface_bright", "surface_bright";
    SurfaceLowest => "surface_lowest", "surface_container_lowest";
    SurfaceLow => "surface_low", "surface_container_low";
    SurfaceContainer => "surface_container", "surface_container";
    SurfaceHigh => "surface_high", "surface_container_high";
    SurfaceHighest => "surface_highest", "surface_container_highest";
    InverseSurface => "inverse_surface", "inverse_surface";
    InverseFg => "inverse_fg", "inverse_on_surface";
    InverseAccent => "inverse_accent", "inverse_primary";
    Outline => "outline", "outline";
    OutlineVariant => "outline_variant", "outline_variant";
    Shadow => "shadow", "shadow";
    Scrim => "scrim", "scrim";
    SurfaceTint => "surface_tint", "surface_tint";
}

impl Role {
    /// The role named `name`, by its Strand name or its Material 3 name.
    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL
            .iter()
            .copied()
            .find(|r| r.name() == name)
            .or_else(|| Role::ALL.iter().copied().find(|r| r.m3() == name))
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
            assert_eq!(Role::from_name(r.m3()), Some(*r));
        }
        let mut names: Vec<_> = Role::ALL.iter().map(|r| r.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Role::COUNT);
        assert_eq!(Role::COUNT, 37);
        assert_eq!(Role::from_name("on_surface"), Some(Role::Fg));
    }
}
