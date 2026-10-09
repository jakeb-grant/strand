//! The logic thread's sleep between steps.

use super::*;

/// What the logic thread's sources collected while it slept.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    pub(super) msgs: Vec<ToLogic>,
    /// Every sender is gone (the main thread ended without a word).
    pub(super) closed: bool,
    /// Loads and settings changes from the compiler worker.
    pub(super) worker: Vec<FromWorker>,
    /// IPC sockets with something to read.
    pub(super) ipc: ipc::Ready,
}

impl ipc::HasReady for Inbox {
    fn ready(&mut self) -> &mut ipc::Ready {
        &mut self.ipc
    }
}

/// The logic thread's sleep: its own calloop loop over the main thread's
/// messages, the runtime's wake hook (a ping) and a `CLOCK_REALTIME`
/// timer for wall-clock wakes, with the logic clock's deadline as the
/// dispatch timeout.
pub(super) struct Sleeper {
    pub(super) event_loop: EventLoop<'static, Inbox>,
    pub(super) timer: Rc<WallTimer>,
}

/// `t` as a `Timespec` since the epoch (before it: the epoch, already
/// past).
pub(super) fn timespec(t: SystemTime) -> Timespec {
    let d = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_nanos(1));
    Timespec {
        tv_sec: d.as_secs() as i64,
        tv_nsec: d.subsec_nanos() as _,
    }
}

impl Sleeper {
    /// The sleeper and the ping that wakes it (the runtime's wake hook).
    #[cfg(test)]
    pub(super) fn new(rx: Channel<ToLogic>) -> io::Result<(Self, calloop::ping::Ping)> {
        Self::with_worker(rx, None)
    }

    /// [`Sleeper::new`], also waking for the compiler worker's results.
    pub(super) fn with_worker(
        rx: Channel<ToLogic>,
        worker: Option<Channel<FromWorker>>,
    ) -> io::Result<(Self, calloop::ping::Ping)> {
        let event_loop = EventLoop::<Inbox>::try_new().map_err(io::Error::other)?;
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |event, _, inbox| match event {
                Event::Msg(m) => inbox.msgs.push(m),
                Event::Closed => inbox.closed = true,
            })
            .map_err(|e| io::Error::other(e.error))?;
        if let Some(w) = worker {
            handle
                .insert_source(w, |event, _, inbox| {
                    if let Event::Msg(m) = event {
                        inbox.worker.push(m);
                    }
                })
                .map_err(|e| io::Error::other(e.error))?;
        }
        let (ping, ping_source) = calloop::ping::make_ping().map_err(io::Error::other)?;
        handle
            .insert_source(ping_source, |_, _, _| {})
            .map_err(|e| io::Error::other(e.error))?;
        let timer = Rc::new(WallTimer::new()?);
        handle
            .insert_source(
                Generic::new(Rc::clone(&timer), Interest::READ, Mode::Level),
                |_, timer, _| {
                    // Expired, or the clock was set (`ECANCELED`): either
                    // way the loop steps with the current wall time.
                    timer.read()?;
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| io::Error::other(e.error))?;
        Ok((Self { event_loop, timer }, ping))
    }

    /// Sleep until a message, a ping, `timeout` (logic clock) or the wall
    /// time `wall` (or a clock step), whichever comes first. `stepped_at`
    /// is the wall time the last step used: a clock set back before the
    /// timer was armed is not reported by `CANCEL_ON_SET`, so it is
    /// checked here.
    pub(super) fn sleep(
        &mut self,
        timeout: Option<Duration>,
        wall: Option<SystemTime>,
        stepped_at: SystemTime,
        inbox: &mut Inbox,
    ) -> io::Result<()> {
        let mut timeout = timeout;
        match wall {
            Some(at) => {
                self.timer.arm_at(timespec(at))?;
                if SystemTime::now() < stepped_at {
                    timeout = Some(Duration::ZERO);
                }
            }
            // Disarmed.
            None => self.timer.arm_at(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })?,
        }
        self.event_loop
            .dispatch(timeout, inbox)
            .map_err(io::Error::other)
    }
}
