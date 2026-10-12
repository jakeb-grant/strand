//! (M5) xdg-activation for actions (docs/architecture.md, "Handler input
//! context"): an action that raises an app (a notification's `invoke`,
//! an app's `launch`: [`Service::ACTIVATION`](crate::Service::ACTIVATION))
//! is sent with a token for the press that started its handler, which
//! the service reads with [`Cx::activation_token`](crate::Cx::activation_token).
//!
//! The token comes from the main thread, which owns the Wayland
//! connection: the binary hands [`Services::set_token_source`](crate::Services::set_token_source)
//! a [`TokenSource`], called on the logic thread with the press's
//! surface and serial, whose future the compositor's answer completes.
//!
//! **Ordering rule.** An action that needs a token waits for it at most
//! [`TOKEN_WAIT`] from when it was submitted (a deadline, so several in
//! one invocation wait together, never one after another), then goes
//! without one. Every later action of the same
//! handler invocation (`ActionCtx::invocation`), to whichever service,
//! with or without an `await` between, queues behind it, so
//! `n.invoke("default"); n.dismiss()` still reaches the notification
//! server in that order. The wait happens on the services' shared
//! runtime; the logic thread never blocks. Invocation 0 (an action
//! outside any handler) is never queued behind anything, nor anything
//! behind it. With no token source, no input or no need, an action goes
//! at once, as before.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::ActionCtx;
use tokio::sync::oneshot;

/// The longest an action waits for its token (m5-owner Q14).
pub const TOKEN_WAIT: Duration = Duration::from_millis(200);

/// A token on its way: `None` when there is none (no protocol, the
/// compositor refused, the main thread is gone).
pub type TokenFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;

/// Asks the main thread for a token for the press `serial` on surface
/// `surface` (`strand_core::InputCtx`'s fields).
pub type TokenSource = Arc<dyn Fn(u32, u32) -> TokenFuture + Send + Sync>;

/// The answering half of a token's one-shot: dropped unanswered, the
/// waiter hears `None`.
#[derive(Debug)]
pub struct TokenAnswer(oneshot::Sender<Option<String>>);

impl TokenAnswer {
    pub fn send(self, token: Option<String>) {
        let _ = self.0.send(token);
    }
}

/// A one-shot for one token: the answer, and the future a
/// [`TokenSource`] returns.
pub fn token_reply() -> (TokenAnswer, TokenFuture) {
    let (tx, rx) = oneshot::channel();
    (
        TokenAnswer(tx),
        Box::pin(async move { rx.await.ok().flatten() }),
    )
}

/// Hands an action to its service, with its token if it got one. The
/// flag says it went later, from a token wait (on the shared runtime):
/// then a run that is gone hands it back to the logic thread and it
/// returns true. Sent now (on the logic thread, the flag false), it
/// returns false when the service's run is gone, for the caller.
pub(crate) type Deliver = Box<dyn FnOnce(Option<String>, bool) -> bool + Send>;

/// A job for the shared runtime (`client.rs`'s `Job`).
pub(crate) type Spawn<'a> =
    &'a dyn Fn(Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>) -> bool;

struct Step {
    /// Its token, and when it stops waiting for it (submit + [`TOKEN_WAIT`]).
    token: Option<(TokenFuture, Instant)>,
    deliver: Deliver,
}

/// One invocation's actions held behind a token wait.
#[derive(Default)]
struct Line {
    steps: VecDeque<Step>,
    /// Drained: later actions of the invocation go at once.
    done: bool,
}

type Shared = Arc<Mutex<Line>>;

fn lock(line: &Shared) -> std::sync::MutexGuard<'_, Line> {
    line.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The services' one ordering queue (one for every service, keyed by
/// invocation), on the logic thread; see the module docs.
#[derive(Default)]
pub(crate) struct Order {
    source: RefCell<Option<TokenSource>>,
    lines: RefCell<HashMap<u64, Shared>>,
}

impl Order {
    pub(crate) fn set_source(&self, source: Option<TokenSource>) {
        *self.source.borrow_mut() = source;
    }

    /// Delivers an action from `ctx` now, or queued behind its
    /// invocation's token wait; `wants` asks a token for it (when the
    /// handler has a press and a token source is set). Returns false
    /// only when delivered now to a run that is gone.
    pub(crate) fn submit(
        &self,
        ctx: ActionCtx,
        wants: bool,
        deliver: Deliver,
        spawn: Spawn<'_>,
    ) -> bool {
        self.lines.borrow_mut().retain(|_, l| !lock(l).done);
        let token = match (wants, ctx.input, self.source.borrow().as_ref()) {
            (true, Some(input), Some(source)) => Some((
                source(input.surface, input.serial),
                Instant::now() + TOKEN_WAIT,
            )),
            _ => None,
        };
        let mut step = Some(Step { token, deliver });
        if ctx.invocation != 0
            && let Some(line) = self.lines.borrow().get(&ctx.invocation)
        {
            let mut l = lock(line);
            if !l.done {
                l.steps.extend(step.take());
            }
        }
        let Some(step) = step else {
            return true;
        };
        if step.token.is_none() {
            return (step.deliver)(None, false);
        }
        let line: Shared = Arc::new(Mutex::new(Line {
            steps: VecDeque::from([step]),
            done: false,
        }));
        if ctx.invocation != 0 {
            self.lines.borrow_mut().insert(ctx.invocation, line.clone());
        }
        let run = line.clone();
        if !spawn(Box::new(move || Box::pin(drain(run)))) {
            // No runtime to wait on: everything goes now, without tokens.
            let steps = std::mem::take(&mut lock(&line).steps);
            lock(&line).done = true;
            for s in steps {
                (s.deliver)(None, true);
            }
        }
        true
    }
}

/// Delivers `line`'s actions in order, each waiting for its token until
/// its deadline ([`TOKEN_WAIT`] after it was submitted); marks it done
/// once empty.
async fn drain(line: Shared) {
    loop {
        let step = {
            let mut l = lock(&line);
            match l.steps.pop_front() {
                Some(s) => s,
                None => {
                    l.done = true;
                    return;
                }
            }
        };
        let token = match step.token {
            Some((t, by)) => tokio::time::timeout_at(tokio::time::Instant::from_std(by), t)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        if !(step.deliver)(token, true) {
            log::debug!("an action queued behind a token wait found its service gone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use strand_core::InputCtx;

    type Log = Arc<Mutex<Vec<(String, Option<String>)>>>;

    fn deliver(log: &Log, what: &str) -> Deliver {
        let (log, what) = (log.clone(), what.to_string());
        Box::new(move |t, _| {
            log.lock().unwrap().push((what, t));
            true
        })
    }

    fn ctx(invocation: u64) -> ActionCtx {
        ActionCtx {
            input: Some(InputCtx {
                surface: 1,
                serial: 9,
            }),
            invocation,
        }
    }

    /// Runs `f` with an `Order` whose spawned waits run on a tokio
    /// runtime in a `LocalSet`, then lets them finish.
    fn with_order(source: Option<TokenSource>, f: impl FnOnce(&Order, Spawn<'_>)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        let order = Order::default();
        order.set_source(source);
        let jobs = RefCell::new(Vec::new());
        let spawn = |job: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>| {
            jobs.borrow_mut().push(job);
            true
        };
        f(&order, &spawn);
        local.block_on(&rt, async {
            let handles: Vec<_> = jobs
                .into_inner()
                .into_iter()
                .map(|j| tokio::task::spawn_local(j()))
                .collect();
            for h in handles {
                h.await.unwrap();
            }
        });
    }

    fn source_after(delay: Duration, token: &'static str) -> TokenSource {
        Arc::new(move |surface, serial| {
            assert_eq!((surface, serial), (1, 9));
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Some(token.to_string())
            })
        })
    }

    /// The action that wants a token gets it, and the invocation's later
    /// actions (another service's too) wait behind it in order; another
    /// invocation's, and invocation 0's, do not.
    #[test]
    fn later_actions_of_an_invocation_wait_behind_its_token() {
        let log: Log = Arc::default();
        with_order(
            Some(source_after(Duration::from_millis(30), "tok")),
            |order, spawn| {
                assert!(order.submit(ctx(5), true, deliver(&log, "invoke"), spawn));
                assert!(order.submit(ctx(5), false, deliver(&log, "dismiss"), spawn));
                assert!(order.submit(ctx(6), false, deliver(&log, "other"), spawn));
                assert!(order.submit(ActionCtx::default(), false, deliver(&log, "ipc"), spawn));
                // The two not held went at once.
                let now: Vec<String> = log.lock().unwrap().iter().map(|e| e.0.clone()).collect();
                assert_eq!(now, ["other", "ipc"]);
            },
        );
        let all = log.lock().unwrap().clone();
        assert_eq!(
            all,
            [
                ("other".into(), None),
                ("ipc".into(), None),
                ("invoke".into(), Some("tok".into())),
                ("dismiss".into(), None),
            ]
        );
    }

    /// A token that never comes holds the action [`TOKEN_WAIT`], then it
    /// goes without one, its followers behind it: nothing dropped,
    /// nothing reordered.
    #[test]
    fn a_token_that_never_comes_costs_the_bound_and_nothing_else() {
        let log: Log = Arc::default();
        let never: TokenSource = Arc::new(|_, _| Box::pin(std::future::pending()));
        let start = Instant::now();
        with_order(Some(never), |order, spawn| {
            order.submit(ctx(7), true, deliver(&log, "launch"), spawn);
            order.submit(ctx(7), false, deliver(&log, "close"), spawn);
        });
        let took = start.elapsed();
        assert!(took >= TOKEN_WAIT, "{took:?}");
        assert!(took < TOKEN_WAIT * 10, "{took:?}");
        assert_eq!(
            *log.lock().unwrap(),
            [("launch".into(), None), ("close".into(), None)]
        );
    }

    /// Several actions of one invocation that want a token, from a
    /// compositor that never answers: each waits [`TOKEN_WAIT`] from its
    /// own submit, so together they cost one bound, not one each, and
    /// still go in order.
    #[test]
    fn token_waits_of_one_invocation_overlap() {
        let log: Log = Arc::default();
        let never: TokenSource = Arc::new(|_, _| Box::pin(std::future::pending()));
        let start = Instant::now();
        with_order(Some(never), |order, spawn| {
            for what in ["a", "b", "c", "d", "e"] {
                order.submit(ctx(11), true, deliver(&log, what), spawn);
            }
            order.submit(ctx(11), false, deliver(&log, "after"), spawn);
        });
        let took = start.elapsed();
        assert!(took >= TOKEN_WAIT, "{took:?}");
        assert!(took < TOKEN_WAIT * 2, "five waits added up: {took:?}");
        let got: Vec<String> = log.lock().unwrap().iter().map(|e| e.0.clone()).collect();
        assert_eq!(got, ["a", "b", "c", "d", "e", "after"]);
    }

    /// No token source (no compositor protocol), or no press: the action
    /// goes at once, with no token.
    #[test]
    fn no_source_or_no_press_means_no_wait() {
        let log: Log = Arc::default();
        with_order(None, |order, spawn| {
            order.submit(ctx(8), true, deliver(&log, "a"), spawn);
            assert_eq!(log.lock().unwrap().len(), 1);
        });
        with_order(
            Some(source_after(Duration::from_secs(5), "x")),
            |order, spawn| {
                let no_press = ActionCtx {
                    input: None,
                    invocation: 9,
                };
                order.submit(no_press, true, deliver(&log, "b"), spawn);
                assert_eq!(log.lock().unwrap().len(), 2);
            },
        );
        assert!(log.lock().unwrap().iter().all(|e| e.1.is_none()));
    }

    /// The one-shot: answered, its future has the token; dropped, `None`.
    #[test]
    fn a_dropped_answer_is_no_token() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (answer, fut) = token_reply();
        answer.send(Some("t".into()));
        assert_eq!(rt.block_on(fut).as_deref(), Some("t"));
        let (answer, fut) = token_reply();
        drop(answer);
        assert_eq!(rt.block_on(fut), None);
    }
}
