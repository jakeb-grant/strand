//! Palettes and theming for Strand (design.md, "Styling, tokens and
//! theming"): the palette schema (Material 3 system roles, 1:1), gamut
//! mapping into sRGB, the contrast guard, `material(seed:)` and
//! `material(image:)` through the `material-colors` crate, the importers
//! (base16/base24, Catppuccin, matugen, W3C design tokens), one
//! derivation table for the roles an importer lacks, and the built-in
//! theme.
//!
//! Colour maths (OKLab/OKLCH, gamut mapping, WCAG contrast) lives in
//! `strand-scene` so the render thread's token evaluator shares it; this
//! crate re-exports it as [`gamut`].

pub mod contrast;
pub mod defaults;
pub mod image;
pub mod import;
pub mod material;
pub mod palette;
pub mod role;
pub mod writer;

pub use contrast::MIN_CONTRAST;
pub use image::{Lookup, Quantiser};
pub use import::{ImportError, import};
pub use material::{Options, Variant, from_seed};
pub use palette::{Palette, Partial};
pub use role::Role;
pub use writer::FileWriter;

/// Gamut mapping and OKLab/OKLCH helpers, shared with the render thread.
pub mod gamut {
    pub use strand_scene::{Color, Oklab, Oklch};

    /// `c` brought into sRGB by lowering OKLCH chroma (CSS Color 4).
    pub fn map(c: Color) -> Color {
        c.gamut_mapped()
    }

    /// Whether `c` lies in sRGB.
    pub fn in_gamut(c: Color) -> bool {
        c.in_gamut(1e-6)
    }
}
