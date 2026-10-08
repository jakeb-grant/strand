//! Reconnecting after a lost socket, without busy loops.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use super::{Cmd, WmError};

/// The first wait after a lost or refused connection.
pub(crate) const FIRST: Duration = Duration::from_millis(100);
/// The longest wait between attempts.
pub(crate) const MAX: Duration = Duration::from_secs(10);
/// How long a connection must have lasted for its loss to start over at
/// [`FIRST`]: one that fails right after connecting (a reply the adapter
/// cannot read, a compositor that closes at once) keeps backing off.
pub(crate) const STABLE: Duration = MAX;

/// Exponential backoff: 100 ms, doubling to 10 s; reset by a connection
/// that stayed up for [`STABLE`].
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
    up_since: Option<Instant>,
}

impl Backoff {
    pub(crate) fn new() -> Self {
        Self {
            next: FIRST,
            up_since: None,
        }
    }

    /// A connection came up.
    pub(crate) fn connected(&mut self) {
        self.up_since = Some(Instant::now());
    }

    /// Waits the current delay (answering commands with `NotConnected`
    /// meanwhile), then doubles it. A connection that lasted [`STABLE`]
    /// first brings the delay back to [`FIRST`].
    pub(crate) async fn wait(&mut self, cmds: &mut UnboundedReceiver<Cmd>) {
        if self.up_since.take().is_some_and(|t| t.elapsed() >= STABLE) {
            self.next = FIRST;
        }
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

        // A connection that fails at once does not start over.
        b.connected();
        tokio::time::advance(Duration::from_millis(50)).await;
        b.wait(&mut rx).await;
        assert_eq!(b.delay(), MAX);

        // One that lasted does.
        b.connected();
        tokio::time::advance(STABLE).await;
        b.wait(&mut rx).await;
        assert_eq!(b.delay(), FIRST * 2, "waited FIRST, then doubled");
    }
}
