//! Waiting for a condition, with a deadline.

use std::fmt::Display;
use std::time::{Duration, Instant};

/// Whether `done` held within `limit`, asked every `poll`. It is asked
/// at least once, and once more after the deadline's last sleep.
pub fn within(limit: Duration, poll: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// Waits for `done` as [`within`] does, and panics with `never: {what}`
/// when it didn't hold in time.
#[track_caller]
pub fn until(what: impl Display, limit: Duration, poll: Duration, done: impl FnMut() -> bool) {
    assert!(within(limit, poll, done), "never: {what}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_already_true_needs_no_time() {
        let at = Instant::now();
        assert!(within(Duration::ZERO, Duration::from_secs(60), || true));
        assert!(at.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn it_asks_until_the_condition_holds() {
        let mut asked = 0;
        assert!(within(Duration::from_secs(60), Duration::ZERO, || {
            asked += 1;
            asked == 3
        }));
        assert_eq!(asked, 3);
    }

    #[test]
    fn a_condition_that_never_holds_is_false_after_the_limit() {
        let limit = Duration::from_millis(20);
        let at = Instant::now();
        assert!(!within(limit, Duration::from_millis(1), || false));
        assert!(at.elapsed() >= limit);
    }

    #[test]
    #[should_panic(expected = "never: the thing")]
    fn until_names_what_never_came() {
        until("the thing", Duration::ZERO, Duration::ZERO, || false);
    }
}
