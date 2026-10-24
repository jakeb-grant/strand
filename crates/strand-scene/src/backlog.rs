//! (history) A `graph`'s backlog: what logic kept of the graph's value
//! before the graph showed it (design.md, "Generative, data-driven and
//! media": `graph X { history: D }` shows the last D of X as soon as it
//! is shown), sent as [`crate::Prop::Backlog`] when the graph mounts and
//! again as the kept history grows.
//!
//! Each point is a value with its age, seconds before the backlog was
//! made (render anchors them at the frame that first draws with it), and
//! how it got there: a point taken while something showed the value
//! steps to it (`ramp` 0: the value held until then); one taken while
//! nothing did, at its service's coarser hidden period, ramps linearly
//! from the point before over that period, so a hidden span reads as a
//! line between its coarse samples.

use std::time::Duration;

/// A `graph`'s `history` when none is set: what logic keeps of its value
/// and what render spans its box with.
pub const DEFAULT_HISTORY: Duration = Duration::from_secs(60);

/// See the module docs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Backlog {
    /// Oldest first (ages falling).
    pub points: Vec<BacklogPoint>,
}

/// One kept value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BacklogPoint {
    /// Seconds before the backlog was made (at least 0).
    pub age: f32,
    pub value: f32,
    /// Seconds, ending at this point, over which the value moved here
    /// from the point before: 0 steps to it.
    pub ramp: f32,
}

impl Backlog {
    /// The value `age` seconds before the backlog was made: the newest
    /// point at or before then, or on its way to the next one while that
    /// one ramps. `None` before the oldest point (nothing kept then).
    pub fn value_at(&self, age: f32) -> Option<f32> {
        if !age.is_finite() {
            return None;
        }
        // Points at or before that moment: ages at least `age`.
        let k = self.points.partition_point(|p| p.age >= age);
        let prev = self.points.get(k.checked_sub(1)?)?;
        let Some(next) = self.points.get(k) else {
            return Some(prev.value);
        };
        let ramp = next.ramp.min(prev.age - next.age);
        let start = next.age + ramp;
        if ramp > 0.0 && age < start {
            let f = ((start - age) / ramp).clamp(0.0, 1.0);
            Some(prev.value + (next.value - prev.value) * f)
        } else {
            Some(prev.value)
        }
    }

    /// The age of the oldest point.
    pub fn oldest(&self) -> Option<f32> {
        self.points.first().map(|p| p.age)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(age: f32, value: f32, ramp: f32) -> BacklogPoint {
        BacklogPoint { age, value, ramp }
    }

    #[test]
    fn shown_points_step_and_hidden_ones_ramp() {
        let b = Backlog {
            points: vec![
                p(10.0, 0.2, 0.0),
                p(9.0, 0.4, 0.0),
                // Hidden from here: 3 s samples, each a ramp.
                p(6.0, 0.7, 3.0),
                p(3.0, 0.1, 3.0),
                // Shown again: a step.
                p(1.0, 0.5, 0.0),
            ],
        };
        assert_eq!(b.value_at(11.0), None, "before the oldest point");
        assert_eq!(b.value_at(10.0), Some(0.2));
        assert_eq!(b.value_at(9.5), Some(0.2), "held until the next step");
        assert_eq!(b.value_at(9.0), Some(0.4));
        assert!((b.value_at(7.5).unwrap() - 0.55).abs() < 1e-6, "halfway up");
        assert!(
            (b.value_at(4.5).unwrap() - 0.4).abs() < 1e-6,
            "halfway down"
        );
        assert_eq!(b.value_at(2.0), Some(0.1), "held before a shown step");
        assert_eq!(b.value_at(0.0), Some(0.5), "the newest holds to now");
        assert_eq!(b.oldest(), Some(10.0));
    }

    #[test]
    fn a_ramp_never_reaches_past_the_point_before() {
        let b = Backlog {
            points: vec![p(2.0, 0.0, 0.0), p(1.0, 1.0, 5.0)],
        };
        assert_eq!(b.value_at(2.0), Some(0.0));
        assert!((b.value_at(1.5).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(Backlog::default().value_at(0.0), None);
    }
}
