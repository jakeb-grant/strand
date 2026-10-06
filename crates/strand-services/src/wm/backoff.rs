//! Reconnecting after a lost socket, without busy loops.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use super::{Cmd, WmError};

/// The first wait after a lost or refused connection.
pub(crate) const FIRST: Duration = Duration::from_millis(100);
/// The longest wait between attempts.
pub(crate) const MAX: Duration = Duration::from_secs(10);

/// Exponential backoff: 100 ms, doubling to 10 s; reset by a connection
/// that came up.
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
}

impl Backoff {
    pub(crate) fn new() -> Self {
        Self { next: FIRST }
    }

    /// A connection came up: the next loss waits the shortest time again.
    pub(crate) fn reset(&mut self) {
        self.next = FIRST;
    }

    /// Waits the current delay (answering commands with `NotConnected`
    /// meanwhile), then doubles it.
    pub(crate) async fn wait(&mut self, cmds: &mut UnboundedReceiver<Cmd>) {
        let sleep = tokio::time::sleep(self.next);
        tokio::pin!(sleep);
        let mut open = true;
        loop {
            tokio::select! {
                () = &mut sleep => break,
                cmd = cmds.recv(), if open => match cmd {
                    Some((_, reply)) => {
                        if let Some(r) = reply {
                            let _ = r.send(Err(WmError::NotConnected));
                        }
                    }
                    None => open = false,
                },
            }
        }
        self.next = (self.next * 2).min(MAX);
    }

    #[cfg(test)]
    pub(crate) fn delay(&self) -> Duration {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn doubles_to_the_cap_and_resets() {
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut b = Backoff::new();
        let mut seen = Vec::new();
        for _ in 0..10 {
            seen.push(b.delay());
            b.wait(&mut rx).await;
        }
        assert_eq!(seen[0], FIRST);
        assert_eq!(seen[1], FIRST * 2);
        assert_eq!(*seen.last().unwrap(), MAX);
        b.reset();
        assert_eq!(b.delay(), FIRST);
    }
}
