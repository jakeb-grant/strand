//! Input forwarded from Wayland. The types live in `strand-scene` so render
//! and logic can name them; see [`strand_scene::input`].

pub use strand_scene::input::{AxisDelta, AxisSource, ButtonState, InputEvent, button};
