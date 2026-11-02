//! Wall-clock gates: what a test may only hold once, at its end, after its
//! functional assertions (CLAUDE.md, "`ci`"). A functional assertion never
//! carries the marker; a budget that depends on the machine always does.

/// The start of every wall-clock gate's failure message. The laptop's
/// advisory timing steps read it (`scripts/container/gate-misses.sh`), and a
/// miss in any other step fails like any other failure.
pub const GATE_MISS: &str = "timing gate missed";

/// A test's wall-clock verdicts, held once at its end ([`Gates::hold`]).
#[derive(Debug, Default)]
pub struct Gates(Vec<String>);

impl Gates {
    /// Notes a miss when `met` is false.
    pub fn check(&mut self, met: bool, miss: impl FnOnce() -> String) {
        if !met {
            self.0.push(miss());
        }
    }

    /// Panics with [`GATE_MISS`] and every miss, if there was one.
    #[track_caller]
    pub fn hold(self) {
        if !self.0.is_empty() {
            panic!("{GATE_MISS}: {}", self.0.join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_the_one_the_container_suite_reads() {
        let check = include_str!("../../../scripts/container/gate-misses.sh");
        assert!(check.contains(&format!("index(msg, \"{GATE_MISS}\") == 1")));
    }

    #[test]
    fn nothing_missed_holds() {
        let mut gates = Gates::default();
        gates.check(true, || "unseen".into());
        gates.hold();
    }

    #[test]
    #[should_panic(expected = "timing gate missed: first\nsecond")]
    fn every_miss_is_named_at_the_end() {
        let mut gates = Gates::default();
        gates.check(false, || "first".into());
        gates.check(true, || "unseen".into());
        gates.check(false, || "second".into());
        gates.hold();
    }
}
