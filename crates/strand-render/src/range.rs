//! (graph-follow) `range`: what a `slider`, `meter`, `arc` or `graph`
//! reads its value against (design.md, "Generative, data-driven and
//! media"; decisions.md graph-follow).
//!
//! `range: lo, hi` maps `lo` to the empty end and `hi` to the full one (a
//! graph's bottom and top); a value outside is drawn at its nearer end.
//! Unset, it is 0 to 1, so a config that never writes it draws as before.
//! `auto`, or `auto` for one end (`range: 0, auto`), follows the values
//! shown: an auto end reaches from the other end (from 0 when both are
//! auto) past every value shown, by a span on the 1, 2, 5 ladder
//! ([`ladder`]). It grows at once when a value goes past it, and narrows
//! only once the values shown fit four fifths of a smaller step
//! ([`Auto::fit`]), so a value wavering about a step does not make the
//! scale jump back and forth. A graph's values shown are its columns; a
//! meter's and an arc's are the values it drew over the last minute
//! ([`Auto::show`]).

use std::collections::VecDeque;

use strand_scene::{Length, PropValue};

/// One end of a range as written: a value, or `auto`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum End {
    At(f32),
    Auto,
}

/// A node's `range` as written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub lo: End,
    pub hi: End,
}

impl Default for Spec {
    fn default() -> Spec {
        Spec {
            lo: End::At(0.0),
            hi: End::At(1.0),
        }
    }
}

fn end(v: &PropValue) -> Option<End> {
    match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) if n.is_finite() => {
            Some(End::At(*n))
        }
        PropValue::Keyword(k) if k == "auto" => Some(End::Auto),
        _ => None,
    }
}

impl Spec {
    /// `range` as a prop value: `auto`, or a list of its two ends; unset,
    /// 0 to 1. `None` for a range written but not usable now: an end that
    /// is not a finite number (an expression over a field not known yet),
    /// which a node draws by its last usable range and a slider writes
    /// nothing in (review: not 0 to 1, which would write fractions into a
    /// place of other units).
    pub fn of(v: Option<&PropValue>) -> Option<Spec> {
        match v {
            None | Some(PropValue::Unset) => Some(Spec::default()),
            Some(PropValue::Keyword(k)) if k == "auto" => Some(Spec {
                lo: End::Auto,
                hi: End::Auto,
            }),
            Some(PropValue::List(items)) => match items.as_slice() {
                [lo, hi] => match (end(lo), end(hi)) {
                    (Some(lo), Some(hi)) => Some(Spec { lo, hi }),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// Either end follows the values shown.
    pub fn is_auto(&self) -> bool {
        self.lo == End::Auto || self.hi == End::Auto
    }

    /// The range of a spec with no auto end (an auto end at its default).
    pub fn fixed(&self) -> Range {
        let at = |e: End, d: f32| match e {
            End::At(v) => v,
            End::Auto => d,
        };
        Range {
            lo: at(self.lo, 0.0),
            hi: at(self.hi, 1.0),
        }
    }
}

/// A range in use: `lo` is the empty end, `hi` the full one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub lo: f32,
    pub hi: f32,
}

impl Range {
    /// 0 to 1.
    pub const UNIT: Range = Range { lo: 0.0, hi: 1.0 };

    /// Where `v` falls, 0 at `lo` and 1 at `hi`, clamped (in an empty
    /// range, 0 up to it and 1 past it); `None` for a value that is not
    /// finite.
    pub fn frac(&self, v: f32) -> Option<f32> {
        if !v.is_finite() {
            return None;
        }
        let span = self.hi - self.lo;
        if span == 0.0 || !span.is_finite() {
            return Some(if v > self.lo { 1.0 } else { 0.0 });
        }
        Some(((v - self.lo) / span).clamp(0.0, 1.0))
    }

    /// The value at fraction `f` of the way from `lo` to `hi` (what a
    /// slider dragged there writes).
    pub fn at(&self, f: f32) -> f32 {
        self.lo + (self.hi - self.lo) * f.clamp(0.0, 1.0)
    }
}

/// The smallest step of the 1, 2, 5 ladder (…, 0.5, 1, 2, 5, 10, 20, …)
/// at or above `x`; `None` for nothing to span (`x` not above 0). A step
/// past the largest `f32` is the largest `f32` (review: `inf` drew every
/// value at the bottom).
pub fn ladder(x: f32) -> Option<f32> {
    if x.is_nan() || x <= 0.0 || !x.is_finite() {
        return None;
    }
    let x = f64::from(x);
    let base = 10f64.powf(x.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * base)
        // Within rounding of a step is that step.
        .find(|s| *s >= x * (1.0 - 1e-6))
        .map(|s| s.min(f64::from(f32::MAX)) as f32)
}

/// How far below a step the values shown must fit before an auto end
/// narrows to it: four fifths.
pub const NARROW: f32 = 0.8;

/// How long a `meter` or `arc` remembers the values it drew for its auto
/// range: a minute, a graph's default `history`.
pub const SHOWN: f64 = 60.0;

/// An auto range's state: the span each auto end reached, and (a meter's
/// or an arc's) the values drawn over the last [`SHOWN`] seconds, as each
/// second's least and greatest. (review) It also keeps a node's last
/// usable `range` ([`Auto::usable`]) and the range it last drew
/// ([`Auto::drawn`]).
#[derive(Clone, Debug, Default)]
pub struct Auto {
    lo: Option<f32>,
    hi: Option<f32>,
    seen: VecDeque<(i64, f32, f32)>,
    good: Option<Spec>,
    drawn: Option<Range>,
}

/// An auto end's span, needing to reach `need` past the other end: up at
/// once to the ladder's step at or above it, down only to a step that
/// `need` fills no more than [`NARROW`] of. `None` when nothing needs
/// spanning (no value shown is past the other end): the end goes back
/// to its default (review: it kept the span a dip had reached long after
/// the dip left the values shown).
fn span(current: Option<f32>, need: f32) -> Option<f32> {
    let up = ladder(need)?;
    match current {
        Some(c) if need <= c => match ladder(need / NARROW) {
            Some(down) if down < c => Some(down),
            _ => Some(c),
        },
        _ => Some(up),
    }
}

impl Auto {
    /// (review) The spec a node draws by: `read` ([`Spec::of`]) when it is
    /// usable, else the last usable one (`None` before any was).
    pub fn usable(&mut self, read: Option<Spec>) -> Option<Spec> {
        if read.is_some() {
            self.good = read;
        }
        self.good
    }

    /// (review) The range last drawn by [`Auto::fit`] or [`Auto::show`]:
    /// what a spring of the node's value settles relative to.
    pub fn drawn(&self) -> Option<Range> {
        self.drawn
    }

    /// The range `spec` shows when the values shown run from `shown`'s
    /// least to its greatest (`None`: nothing shown, which moves no auto
    /// end), the auto ends moved as the module docs say.
    pub fn fit(&mut self, spec: Spec, shown: Option<(f32, f32)>) -> Range {
        let r = self.fit_ends(spec, shown);
        self.drawn = Some(r);
        r
    }

    fn fit_ends(&mut self, spec: Spec, shown: Option<(f32, f32)>) -> Range {
        if let Some((min, max)) = shown.filter(|(a, b)| a.is_finite() && b.is_finite()) {
            match (spec.lo, spec.hi) {
                (End::At(lo), End::Auto) => self.hi = span(self.hi, max - lo),
                (End::Auto, End::At(hi)) => self.lo = span(self.lo, hi - min),
                (End::Auto, End::Auto) => {
                    // From 0, each way the values go.
                    self.hi = span(self.hi, max);
                    self.lo = span(self.lo, -min);
                }
                (End::At(_), End::At(_)) => {}
            }
        }
        match (spec.lo, spec.hi) {
            (End::At(lo), End::At(hi)) => Range { lo, hi },
            (End::At(lo), End::Auto) => Range {
                lo,
                hi: lo + self.hi.unwrap_or(1.0),
            },
            (End::Auto, End::At(hi)) => Range {
                lo: hi - self.lo.unwrap_or(1.0),
                hi,
            },
            (End::Auto, End::Auto) => {
                let (lo, hi) = (-self.lo.unwrap_or(0.0), self.hi.unwrap_or(0.0));
                if hi > lo {
                    Range { lo, hi }
                } else {
                    Range::UNIT
                }
            }
        }
    }

    /// (`meter`, `arc`) `v` drawn at `now` (seconds on the presentation
    /// clock): the range `spec` shows over the values drawn in the last
    /// [`SHOWN`] seconds.
    pub fn show(&mut self, spec: Spec, now: f64, v: f32) -> Range {
        if v.is_finite() {
            let sec = now.floor() as i64;
            match self.seen.back_mut() {
                Some((s, lo, hi)) if *s == sec => {
                    *lo = lo.min(v);
                    *hi = hi.max(v);
                }
                _ => self.seen.push_back((sec, v, v)),
            }
        }
        let oldest = (now - SHOWN).floor() as i64;
        while self.seen.front().is_some_and(|(s, _, _)| *s < oldest) {
            self.seen.pop_front();
        }
        let shown = self
            .seen
            .iter()
            .fold(None, |acc: Option<(f32, f32)>, (_, lo, hi)| {
                Some(acc.map_or((*lo, *hi), |(a, b)| (a.min(*lo), b.max(*hi))))
            });
        self.fit(spec, shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> Spec {
        Spec {
            lo: End::Auto,
            hi: End::Auto,
        }
    }

    #[test]
    fn the_ladder_is_one_two_five() {
        let l = |x| ladder(x).unwrap();
        assert_eq!(l(0.37), 0.5);
        assert_eq!(l(1.0), 1.0);
        assert_eq!(l(1.2), 2.0);
        assert_eq!(l(2.01), 5.0);
        assert_eq!(l(7.0), 10.0);
        assert_eq!(l(1234.0), 2000.0);
        assert_eq!(ladder(0.0), None);
        assert_eq!(ladder(f32::NAN), None);
        // (review) Past the largest float: the largest float, not `inf`.
        assert_eq!(l(3.0e38), f32::MAX);
        let r = Auto::default().fit(auto(), Some((0.0, 3.0e38)));
        assert_eq!(
            r,
            Range {
                lo: 0.0,
                hi: f32::MAX
            }
        );
        assert!(r.frac(1.5e38).is_some_and(|f| (f - 0.44).abs() < 0.01));
    }

    #[test]
    fn a_spec_reads_two_ends_or_auto() {
        let n = PropValue::Number;
        let kw = || PropValue::Keyword("auto".into());
        assert_eq!(Spec::of(None), Some(Spec::default()));
        assert_eq!(Spec::of(Some(&kw())), Some(auto()));
        assert_eq!(
            Spec::of(Some(&PropValue::List(vec![n(0.0), kw()]))),
            Some(Spec {
                lo: End::At(0.0),
                hi: End::Auto
            })
        );
        assert_eq!(
            Spec::of(Some(&PropValue::List(vec![n(10.0), n(30.0)]))).map(|s| s.fixed()),
            Some(Range { lo: 10.0, hi: 30.0 })
        );
        assert_eq!(Spec::of(Some(&n(5.0))), None, "one end: not usable");
    }

    /// (review) An end that is not a finite number (a field not known
    /// yet) makes the range unusable, not 0 to 1: a node keeps drawing by
    /// its last usable range.
    #[test]
    fn an_end_not_known_keeps_the_last_usable_range() {
        let n = PropValue::Number;
        let ends = |hi: f32| PropValue::List(vec![n(0.0), n(hi)]);
        assert_eq!(Spec::of(Some(&ends(f32::NAN))), None);
        assert_eq!(
            Spec::of(Some(&PropValue::List(vec![n(0.0), PropValue::Unset]))),
            None
        );
        let mut a = Auto::default();
        assert_eq!(a.usable(Spec::of(Some(&ends(f32::NAN)))), None, "none yet");
        let good = a.usable(Spec::of(Some(&ends(100.0))));
        assert_eq!(good.map(|s| s.fixed()), Some(Range { lo: 0.0, hi: 100.0 }));
        assert_eq!(a.usable(Spec::of(Some(&ends(f32::INFINITY)))), good);
    }

    #[test]
    fn values_outside_clamp_and_a_slider_writes_in_range() {
        let r = Range { lo: 0.0, hi: 100.0 };
        assert_eq!(r.frac(25.0), Some(0.25));
        assert_eq!(r.frac(140.0), Some(1.0));
        assert_eq!(r.frac(-3.0), Some(0.0));
        assert_eq!(r.frac(f32::NAN), None);
        assert_eq!(r.at(0.4), 40.0);
        assert_eq!(Range { lo: 5.0, hi: 5.0 }.frac(6.0), Some(1.0), "empty");
    }

    /// An auto top grows at once past a value, holds while the values
    /// waver about a step, and narrows once they fit four fifths of a
    /// smaller one.
    #[test]
    fn an_auto_top_grows_at_once_and_narrows_with_a_margin() {
        let mut a = Auto::default();
        let s = auto();
        let top = |a: &mut Auto, max: f32| a.fit(s, Some((0.0, max))).hi;
        assert_eq!(top(&mut a, 1.7), 2.0);
        assert_eq!(top(&mut a, 2.01), 5.0, "past the step: up at once");
        assert_eq!(top(&mut a, 1.9), 5.0, "1.9 is more than 4/5 of 2");
        assert_eq!(top(&mut a, 2.2), 5.0);
        assert_eq!(top(&mut a, 1.6), 2.0, "4/5 of 2: down");
        assert_eq!(top(&mut a, 1.7), 2.0);
        assert_eq!(top(&mut a, 0.3), 0.5);
        // Nothing shown: 0 to 1; values below zero reach down.
        assert_eq!(Auto::default().fit(s, None), Range::UNIT);
        assert_eq!(
            Auto::default().fit(s, Some((-3.0, 4.0))),
            Range { lo: -5.0, hi: 5.0 }
        );
    }

    /// (review) An auto end goes back once nothing shown reaches past
    /// the other end: a dip below zero widens the bottom, and once it has
    /// left the values shown the bottom is 0 again; the same for a top
    /// once every value is at or below the bottom.
    #[test]
    fn an_auto_end_goes_back_once_nothing_reaches_it() {
        let mut a = Auto::default();
        let s = auto();
        assert_eq!(a.fit(s, Some((0.2, 0.8))), Range { lo: 0.0, hi: 1.0 });
        assert_eq!(a.fit(s, Some((-4.0, 0.8))), Range { lo: -5.0, hi: 1.0 });
        assert_eq!(a.fit(s, Some((0.1, 0.8))), Range { lo: 0.0, hi: 1.0 });
        // Nothing shown moves nothing.
        assert_eq!(a.fit(s, Some((-4.0, 0.8))), Range { lo: -5.0, hi: 1.0 });
        assert_eq!(a.fit(s, None), Range { lo: -5.0, hi: 1.0 });
        let top = Spec {
            lo: End::At(10.0),
            hi: End::Auto,
        };
        let mut b = Auto::default();
        assert_eq!(b.fit(top, Some((10.0, 470.0))).hi, 510.0);
        assert_eq!(b.fit(top, Some((2.0, 9.0))).hi, 11.0, "back to 1 past it");
    }

    #[test]
    fn one_auto_end_spans_from_the_other() {
        let spec = Spec {
            lo: End::At(30.0),
            hi: End::Auto,
        };
        let r = Auto::default().fit(spec, Some((40.0, 61.0)));
        assert_eq!(r, Range { lo: 30.0, hi: 80.0 }, "a span of 50 past 30");
        let spec = Spec {
            lo: End::Auto,
            hi: End::At(100.0),
        };
        assert_eq!(
            Auto::default().fit(spec, Some((85.0, 90.0))),
            Range {
                lo: 80.0,
                hi: 100.0
            }
        );
    }

    /// A meter's auto range follows the values it drew in the last
    /// minute: a burst widens it, and it narrows once the burst is a
    /// minute old and a value is drawn.
    #[test]
    fn a_meters_auto_range_forgets_after_a_minute() {
        let mut a = Auto::default();
        let s = Spec {
            lo: End::At(0.0),
            hi: End::Auto,
        };
        assert_eq!(a.show(s, 100.0, 40.0).hi, 50.0);
        assert_eq!(a.show(s, 101.5, 900.0).hi, 1000.0);
        assert_eq!(a.show(s, 150.0, 30.0).hi, 1000.0, "the burst is recent");
        assert_eq!(a.show(s, 162.0, 30.0).hi, 50.0, "a minute on");
    }
}
