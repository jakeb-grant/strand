//! (lock-int) Before a lock's stills are taken, Strand's own surfaces
//! settle, so the still is the desktop at rest (design.md, "Lock
//! screen"; docs/architecture.md, "Session lock"; decisions.md lock-int).
//!
//! A `lock` spec that opens asks the host to settle first
//! ([`SurfaceHost::settle_for_lock`]): render ends every running `exit`
//! outside the lock and marks the other surfaces with a motion toward an
//! end for one settled frame. With nothing to settle the lock is asked
//! for at once, as before, adding no wait. Otherwise the request waits
//! until each surface the host named is settled: its settled frame
//! committed (painted at once, not after a frame callback still owed for
//! the frame before: a commit without one is allowed, and a surface that
//! animates always owes one), a GPU-presented one's presented by the GPU
//! thread (the host owes it until then); or gone (its spec closed, a
//! GPU-presented one only once the GPU thread let go of it). Then, once
//! the host has handed over every surface change, a `wl_display.sync`
//! sent after all those commits and destroys must come back, so the
//! compositor has them before the capture, which runs on a connection
//! of its own. No frame callback of the settled frame is waited for: that
//! is a refresh more (up to 17 ms at 60 Hz, on top of the frame before's)
//! and the capture is of the compositor's next frame anyway.
//! [`LOCK_SETTLE_CAP`] bounds the wait by a timer of the manager's own,
//! whatever the host or the compositor does. Then the lock is asked for
//! as [`State::lock`] asks it: the host's `before_lock` takes the stills,
//! and the request goes.
//!
//! Only a lock asked for by its spec opening settles: [`State::lock`]
//! called directly (a fail-closed lock after a restart, or the lock asked
//! for again after `finished`) is asked for at once, and ends a settle
//! in progress at once. The spec closing (or going) meanwhile ends the
//! settle and asks for the lock all the same: once a spec has opened, the
//! lock is asked for, as when there was no settle, and only an unlock
//! token ends it. However it ends, the host hears it
//! ([`SurfaceHost::lock_settle_ended`]) and drops what it marked. The
//! lock's own surfaces are never settled.

use std::collections::BTreeSet;

use wayland_client::protocol::wl_callback::{self, WlCallback};

use super::*;

/// (lock-int) The longest a lock waits for Strand's own surfaces to
/// settle before its stills are taken: about one refresh and a half at
/// 60 Hz, so the lock's first frame stays fast. The stills are taken at
/// this cap whatever has shown.
pub const LOCK_SETTLE_CAP: Duration = Duration::from_millis(25);

/// (lock-int) What a host settled before a lock's stills
/// ([`SurfaceHost::settle_for_lock`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockSettled {
    /// The surfaces the stills wait for: each paints its settled frame
    /// (until [`SurfaceHost::lock_settle_owed`] is false) and that frame
    /// shows, or it goes.
    pub surfaces: Vec<SurfaceId>,
    /// What was settled, for the log line.
    pub what: String,
}

/// (lock-int) How a lock's settle ended
/// ([`SurfaceHost::lock_settle_ended`]). The lock is asked for after each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockSettleEnd {
    /// Every surface settled and the sync after them came back.
    Settled,
    /// [`LOCK_SETTLE_CAP`] came first (a warning says what was owed).
    Cap,
    /// The lock's spec closed or went meanwhile.
    Closed,
    /// [`State::lock`] was called directly meanwhile.
    Stopped,
}

/// A settle in progress.
#[derive(Debug)]
pub(super) struct Settle {
    since: Instant,
    /// The cap's timer ([`LOCK_SETTLE_CAP`]).
    timer: Option<RegistrationToken>,
    /// The surfaces not settled yet.
    waited: BTreeSet<SurfaceId>,
    /// How many were waited for.
    count: usize,
    what: String,
    /// The `wl_display.sync` sent once every surface settled.
    sync: Option<WlCallback>,
    /// A look is due once the current dispatch is through.
    look_due: bool,
}

/// User data of the settle's `wl_display.sync`.
#[derive(Debug)]
pub struct SettleSync;

impl<H: SurfaceHost + 'static> State<H> {
    /// (lock-int) A lock spec opened: Strand's own surfaces settle before
    /// the lock is asked for (module docs). True if a settle started (or
    /// one is in progress); false when the host had nothing to settle,
    /// and the caller asks for the lock at once.
    pub(super) fn settle_before_lock(&mut self) -> bool {
        if self.lock_settle.is_some() {
            return true;
        }
        let Some(settled) = self.host.settle_for_lock() else {
            return false;
        };
        let timer = self.handle.insert_source(
            Timer::from_duration(LOCK_SETTLE_CAP),
            |_, _, state: &mut State<H>| {
                if let Some(s) = &mut state.lock_settle {
                    s.timer = None;
                }
                state.end_settle(LockSettleEnd::Cap);
                TimeoutAction::Drop
            },
        );
        let timer = match timer {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "lock: no timer for the settle's cap ({}): taking the stills now",
                    e.error
                );
                return false;
            }
        };
        let waited: BTreeSet<SurfaceId> = settled.surfaces.iter().copied().collect();
        // Their settled frames are drawn at the next flush.
        for id in &waited {
            if self.surfaces.contains_key(id) {
                self.mark(*id);
            }
        }
        self.lock_settle = Some(Settle {
            since: Instant::now(),
            timer: Some(timer),
            count: waited.len(),
            waited,
            what: settled.what,
            sync: None,
            look_due: false,
        });
        self.settle_soon();
        true
    }

    /// (lock-int) A lock is settling.
    pub fn lock_settling(&self) -> bool {
        self.lock_settle.is_some()
    }

    /// (lock-int) Something a settle waits for may have changed (a paint,
    /// a frame shown, a surface gone or taken back, surface changes
    /// handed over): it looks again once the current dispatch is through,
    /// when the frame just painted has gone with its callback asked.
    pub(super) fn settle_soon(&mut self) {
        let Some(s) = &mut self.lock_settle else {
            return;
        };
        if s.look_due {
            return;
        }
        s.look_due = true;
        self.handle.insert_idle(|state| {
            if let Some(s) = &mut state.lock_settle {
                s.look_due = false;
            }
            state.settle_look();
        });
    }

    /// (lock-int) Lets a settle look again: the host calls it when what
    /// it owed for a surface is done outside the manager (the GPU thread
    /// presented its settled frame).
    pub fn lock_settle_look(&mut self) {
        self.settle_soon();
    }

    /// (lock-int) A settle waits for `id`'s settled frame, still owed: it
    /// is painted and committed at once, a frame callback still owed for
    /// the frame before notwithstanding (`commit.rs`), and a GPU-presented
    /// one is sent at once where the WSI does not wait for callbacks.
    pub(super) fn settle_owes(&self, id: SurfaceId) -> bool {
        self.lock_settle
            .as_ref()
            .is_some_and(|s| s.waited.contains(&id))
            && self.host.lock_settle_owed(id)
    }

    /// Whether `id` is settled: gone (a GPU-presented one once the GPU
    /// thread let go of it), never mapped, or its settled frame committed
    /// (the host owes nothing for it: a GPU-presented one is owed until
    /// the GPU thread presented it). Its frame callback is not waited for
    /// (module docs).
    fn settled(&self, id: SurfaceId) -> bool {
        match self.surfaces.get(&id) {
            None => !self.gpu.is_doomed(id),
            Some(s) => !s.mapped() || !self.host.lock_settle_owed(id),
        }
    }

    /// Drops what has settled; once nothing is left and the host has
    /// handed over its surface changes, sends the sync.
    fn settle_look(&mut self) {
        let Some(s) = &self.lock_settle else {
            return;
        };
        let left: BTreeSet<SurfaceId> = s
            .waited
            .iter()
            .copied()
            .filter(|id| !self.settled(*id))
            .collect();
        let changes = self.host.lock_settle_changes();
        let Some(s) = &mut self.lock_settle else {
            return;
        };
        s.waited = left;
        if !s.waited.is_empty() || changes || s.sync.is_some() {
            return;
        }
        s.sync = Some(self.conn.display().sync(&self.qh, SettleSync));
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the settle's sync failed: {e}");
        }
    }

    /// The settle's sync came back: the compositor has everything.
    fn settle_synced(&mut self, callback: &WlCallback) {
        if self
            .lock_settle
            .as_ref()
            .is_some_and(|s| s.sync.as_ref() == Some(callback))
        {
            self.end_settle(LockSettleEnd::Settled);
        }
    }

    /// The settle ended (`how`): the lock is asked for now, its stills
    /// taken first.
    pub(super) fn end_settle(&mut self, how: LockSettleEnd) {
        let Some(s) = self.take_settle(how) else {
            return;
        };
        let ms = s.since.elapsed().as_secs_f64() * 1000.0;
        match how {
            LockSettleEnd::Cap => {
                let owed: Vec<String> = s.waited.iter().map(|id| id.0.to_string()).collect();
                log::warn!(
                    "lock: Strand's surfaces had not settled at the {} ms cap ({}; surfaces {} \
                     owed): taking the stills",
                    LOCK_SETTLE_CAP.as_millis(),
                    s.what,
                    if owed.is_empty() {
                        "none".to_string()
                    } else {
                        owed.join(", ")
                    },
                );
            }
            LockSettleEnd::Settled => log::info!(
                "lock: Strand's surfaces settled in {ms:.1} ms before the stills ({}; {} surfaces)",
                s.what,
                s.count
            ),
            LockSettleEnd::Closed => log::info!(
                "lock: closed again after {ms:.1} ms as Strand's surfaces settled: asked for all \
                 the same"
            ),
            LockSettleEnd::Stopped => {}
        }
        if let Err(e) = self.lock() {
            log::warn!("{e}");
        }
    }

    /// (lock-int) The lock spec closed or went while it settled: the lock
    /// is asked for all the same, as it was before there was a settle (a
    /// spec that opened asks for the lock; only a token unlocks).
    pub(super) fn close_settle(&mut self) {
        self.end_settle(LockSettleEnd::Closed);
    }

    /// (lock-int) A lock asked for directly ([`State::lock`]) goes at
    /// once: a settle in progress stops.
    pub(super) fn stop_settle(&mut self) {
        if self.take_settle(LockSettleEnd::Stopped).is_some() {
            log::info!("lock: asked for at once: Strand's surfaces stop settling");
        }
    }

    /// Ends a settle in progress (`how`), its timer with it, and tells the
    /// host, which drops what it marked.
    fn take_settle(&mut self, how: LockSettleEnd) -> Option<Settle> {
        let s = self.lock_settle.take()?;
        if let Some(t) = s.timer {
            self.handle.remove(t);
        }
        self.host.lock_settle_ended(how);
        Some(s)
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WlCallback, State<H>> for SettleSync {
    fn event(
        &self,
        state: &mut State<H>,
        callback: &WlCallback,
        event: wl_callback::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.settle_synced(callback);
        }
    }
}
