//! The event channel and the tasks that feed it.
//!
//! Everything that can change the screen arrives as an [`AppEvent`] on one
//! `mpsc` channel, consumed by one loop that owns the state. There is no lock over
//! application state anywhere in the program, and that is a consequence of this shape
//! rather than a discipline anyone has to maintain.
//!
//! # Why one channel
//!
//! The alternative — tasks holding an `Arc<Mutex<State>>` and mutating it directly —
//! makes every render a race against a background fetch, and makes "what changed?"
//! unanswerable, which is exactly what redraw-on-change needs to know. Funnelling
//! through a channel means the loop sees every change in order and can decide once per
//! frame whether anything is worth drawing.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::gitlab::fetch::Snapshot;

/// How often background changes may redraw.
///
/// The tick caps snapshot, clock and resize work so a storm of them collapses into one
/// frame. Key input does not wait for it: input latency is what a user can feel, and the
/// loop paints a changed key immediately.
pub const RENDER_INTERVAL: Duration = Duration::from_millis(100);

/// How often relative times are recomputed. The clock is separate from the render tick
/// because "3m ago" changing is a state change, while a render tick is not.
pub const CLOCK_INTERVAL: Duration = Duration::from_secs(1);

/// Which configured filter an event belongs to, by index into the filter list.
pub type FilterId = usize;

/// Everything that can change what is on screen.
#[derive(Debug)]
pub enum AppEvent {
    /// A key, mouse, paste, focus or resize event from the terminal.
    Input(crossterm::event::Event),

    /// Time to consider redrawing.
    RenderTick,

    /// Time to recompute relative times and refresh countdowns.
    ClockTick,

    /// A filter finished fetching.
    ///
    /// The whole [`Snapshot`] travels rather than its rows: truncation, partiality and
    /// the query shape that worked are three unrelated conditions, and the UI says
    /// something different about each.
    Snapshot {
        filter: FilterId,
        snapshot: Box<Snapshot>,
    },

    /// A filter's fetch failed. The previous snapshot stays on screen.
    FetchFailed { filter: FilterId, error: Box<Error> },

    /// A filter started fetching, so the status bar can show the spinner.
    FetchStarted { filter: FilterId },

    /// When a filter's next refresh is due, so the status bar can count down to it.
    ///
    /// Published by the scheduler rather than derived in the UI: the wait is
    /// `interval + jitter` on success and an exponential backoff on failure, and a
    /// status bar that recomputed it would be confidently wrong.
    RefreshScheduled { filter: FilterId, due: Instant },

    /// A runtime 401/403: every filter stops refreshing until the user asks again.
    RefreshesPaused { filter: FilterId },

    /// The identity probe landed. The rows the cache put on screen carry whichever
    /// account's derived flags the run that wrote them computed; this is where they are
    /// re-derived against the account this session actually authenticated as.
    Identified { username: String },

    /// The identity probe failed. Fatal, by the same recovery table that made it fatal
    /// when it ran before the guard — the only difference now is that the guard is up,
    /// so the message has to travel out of `run` and `main` prints it to the restored
    /// terminal instead of an `eprintln` reaching the alternate screen.
    ///
    /// Boxed for the same reason `FetchFailed` is: `AppEvent` pays the size of its
    /// biggest variant on every event (see `the_event_enum_stays_small`).
    IdentityFailed { error: Box<Error> },

    /// Shut down: a signal arrived, or the user quit.
    Quit(QuitReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// The filter it came from, so the user knows which tab to open.
    pub filter: Option<FilterId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    /// The user pressed a quit binding.
    User,
    /// SIGINT, SIGTERM or SIGHUP.
    Signal(crate::term::signal::Termination),
    /// A background task panicked. Tokio caught it, so the process is still alive, but
    /// the panic hook (`term::guard`) already restored the terminal — continuing to draw
    /// would paint onto a screen the loop no longer owns, so this is a quit rather than a
    /// crash.
    TaskPanicked,
    /// Startup could not complete: the identity probe failed after the terminal was
    /// already taken over. The error itself travels on `App::fatal`, not in this
    /// variant, so `QuitReason` stays `Copy` — a boxed error here would cost `Flow`,
    /// `main` and every test that compares a reason.
    Fatal,
}

/// The sending half, cloned into every task that can produce an event.
pub type EventSender = mpsc::UnboundedSender<AppEvent>;

/// The receiving half, owned by the loop.
pub type EventReceiver = mpsc::UnboundedReceiver<AppEvent>;

/// Create the channel.
///
/// Unbounded deliberately. A bounded channel would make a producer wait on the loop, and
/// the producer that matters is the terminal input task: dropping or delaying keystrokes
/// because a fetch is in flight is the one thing a keyboard-driven tool must never do.
/// The volume is tiny — a few events per second — so unbounded costs nothing here.
pub fn channel() -> (EventSender, EventReceiver) {
    mpsc::unbounded_channel()
}

/// Owns the background tasks and the token that stops them.
#[derive(Debug)]
pub struct Tasks {
    cancel: CancellationToken,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Tasks {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            handles: Vec::new(),
        }
    }

    /// A child token, for a task that should stop when the application does.
    pub fn token(&self) -> CancellationToken {
        self.cancel.child_token()
    }

    pub fn track(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    /// Stop every task and wait for it to finish.
    ///
    /// Awaited rather than fire-and-forget: the terminal guard restores on drop, and a
    /// task still writing when that happens would paint onto the restored screen. The
    /// visible symptom is a corrupted shell prompt after quitting.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for handle in self.handles {
            // A task that returns normally on cancellation is `Ok`; nothing here ever
            // calls `.abort()`, so an `Err` is always a panic, and the last-resort quit
            // path above is the only reason the loop is still running to see it.
            if let Err(error) = handle.await {
                tracing::error!(%error, "a background task panicked");
            }
        }
    }
}

impl Default for Tasks {
    fn default() -> Self {
        Self::new()
    }
}

/// Forward terminal events until cancelled.
pub fn spawn_input(tasks: &mut Tasks, events: EventSender) {
    use futures_util::StreamExt;

    let cancel = tasks.token();
    tasks.track(tokio::spawn(async move {
        let mut stream = crossterm::event::EventStream::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                maybe = stream.next() => match maybe {
                    Some(Ok(event)) => {
                        if events.send(AppEvent::Input(event)).is_err() {
                            break;
                        }
                    }
                    // A read error usually means the terminal went away, which the
                    // signal handler is about to act on anyway.
                    Some(Err(error)) => {
                        tracing::debug!(%error, "terminal input stream failed");
                        break;
                    }
                    None => break,
                },
            }
        }
    }));
}

/// Drive the render and clock ticks.
///
/// One task for both, because they are the same kind of thing and two timers would only
/// mean two wakeups where one will do.
pub fn spawn_ticks(tasks: &mut Tasks, events: EventSender) {
    spawn_ticks_with(tasks, events, crate::term::guard::task_panicked);
}

/// [`spawn_ticks`], with the panic check injected.
///
/// A real run has exactly one of these tasks, so reading the global flag directly would
/// be harmless there — but `cargo test` runs every test's tick task in one process, all
/// polling the same global, so one test flagging a panic could be observed by another
/// test's task. Injecting the check is what keeps the tests independent of each other,
/// the same reason `Env` and `TokenEnv` are injected instead of read from the process.
fn spawn_ticks_with(
    tasks: &mut Tasks,
    events: EventSender,
    panicked: impl Fn() -> bool + Send + 'static,
) {
    let cancel = tasks.token();
    tasks.track(tokio::spawn(async move {
        let mut render = tokio::time::interval(RENDER_INTERVAL);
        let mut clock = tokio::time::interval(CLOCK_INTERVAL);
        // Skip missed ticks rather than firing them back to back: a laptop resuming
        // from sleep would otherwise deliver a burst of hundreds.
        render.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = render.tick() => {
                    // A panicked task already tore down the terminal (term::guard);
                    // checked here, on the tick that already drives the redraw cadence,
                    // rather than adding a second timer just to poll a flag.
                    if panicked() {
                        let _ = events.send(AppEvent::Quit(QuitReason::TaskPanicked));
                        break;
                    }
                    if events.send(AppEvent::RenderTick).is_err() {
                        break;
                    }
                }
                _ = clock.tick() => {
                    if events.send(AppEvent::ClockTick).is_err() {
                        break;
                    }
                }
            }
        }
    }));
}

/// Turn a termination signal into a quit event.
///
/// `signals` is `None` when [`crate::term::signal::Signals::register`] failed — logged by
/// the caller already — in which case there is nothing to wait on and the signals keep
/// their default disposition for the session.
pub fn spawn_signals(
    tasks: &mut Tasks,
    events: EventSender,
    signals: Option<crate::term::signal::Signals>,
) {
    let Some(mut signals) = signals else {
        return;
    };
    let cancel = tasks.token();
    tasks.track(tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {}
            signal = signals.terminated() => {
                tracing::info!(signal = signal.as_str(), "shutting down");
                let _ = events.send(AppEvent::Quit(QuitReason::Signal(signal)));
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::signal::Termination;

    #[test]
    fn render_and_clock_intervals_match_the_spec() {
        assert_eq!(RENDER_INTERVAL, Duration::from_millis(100));
        assert_eq!(CLOCK_INTERVAL, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn events_arrive_in_the_order_they_were_sent() {
        let (tx, mut rx) = channel();

        tx.send(AppEvent::FetchStarted { filter: 0 }).unwrap();
        tx.send(AppEvent::RenderTick).unwrap();
        tx.send(AppEvent::Quit(QuitReason::User)).unwrap();

        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::FetchStarted { filter: 0 })
        ));
        assert!(matches!(rx.recv().await, Some(AppEvent::RenderTick)));
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::Quit(QuitReason::User))
        ));
    }

    /// Dropping the loop's receiver must not panic a producer — it is what happens
    /// during shutdown, when tasks may still be mid-send.
    #[tokio::test]
    async fn sending_after_the_receiver_is_gone_is_an_error_not_a_panic() {
        let (tx, rx) = channel();
        drop(rx);

        assert!(tx.send(AppEvent::RenderTick).is_err());
    }

    #[tokio::test]
    async fn ticks_arrive_at_roughly_the_configured_rate() {
        let (tx, mut rx) = channel();
        let mut tasks = Tasks::new();
        spawn_ticks(&mut tasks, tx);

        let mut renders = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(350);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(AppEvent::RenderTick)) => renders += 1,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        tasks.shutdown().await;

        // ~3 in 350ms; the bounds are loose because CI machines are not real-time.
        assert!(
            (2..=6).contains(&renders),
            "expected roughly 3 render ticks, got {renders}"
        );
    }

    /// The clock ticks ten times less often than the render loop, so relative times are
    /// recomputed once a second rather than ten times.
    #[tokio::test]
    async fn clock_ticks_are_rarer_than_render_ticks() {
        let (tx, mut rx) = channel();
        let mut tasks = Tasks::new();
        spawn_ticks(&mut tasks, tx);

        let (mut renders, mut clocks) = (0, 0);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(AppEvent::RenderTick)) => renders += 1,
                Ok(Some(AppEvent::ClockTick)) => clocks += 1,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        tasks.shutdown().await;

        assert!(renders > clocks, "renders {renders}, clocks {clocks}");
    }

    /// The panic hook already restored the terminal by the time this fires; the tick
    /// task must quit rather than keep sending ticks the loop would draw onto it with.
    ///
    /// A private flag, not the real global: every test's tick task polls whichever flag
    /// it was given, and the real one is shared process-wide across every test binary
    /// running concurrently.
    #[tokio::test]
    async fn a_flagged_panic_quits_instead_of_ticking() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let (tx, mut rx) = channel();
        let mut tasks = Tasks::new();

        let panicked = Arc::new(AtomicBool::new(true));
        let check = Arc::clone(&panicked);
        spawn_ticks_with(&mut tasks, tx, move || {
            check.swap(false, std::sync::atomic::Ordering::SeqCst)
        });

        // The render and clock timers both fire immediately on creation, so a `ClockTick`
        // may legitimately arrive before the render tick that carries the quit — only the
        // absence of a prompt `Quit` at all would be the bug.
        let reason = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Some(AppEvent::Quit(reason)) => return Some(reason),
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("a quit should arrive promptly")
        .expect("the channel should not close before a quit arrives");

        assert_eq!(reason, QuitReason::TaskPanicked);
        tasks.shutdown().await;
    }

    /// Shutdown must be awaited, not fired and forgotten: the terminal guard restores on
    /// drop, and a task still running then paints onto the restored screen.
    #[tokio::test]
    async fn shutdown_stops_every_task() {
        let (tx, _rx) = channel();
        let mut tasks = Tasks::new();
        spawn_ticks(&mut tasks, tx.clone());
        let signals = crate::term::signal::Signals::register().ok();
        spawn_signals(&mut tasks, tx, signals);

        tokio::time::timeout(Duration::from_secs(5), tasks.shutdown())
            .await
            .expect("shutdown should not hang");
    }

    /// `spawn_signals` with no registered handlers tracks no task at all, rather than one
    /// that can never resolve — `Tasks::shutdown` must not wait on it.
    #[tokio::test]
    async fn spawn_signals_with_no_registration_tracks_nothing() {
        let (tx, _rx) = channel();
        let mut tasks = Tasks::new();
        spawn_signals(&mut tasks, tx, None);

        tokio::time::timeout(Duration::from_secs(5), tasks.shutdown())
            .await
            .expect("shutdown should not hang");
    }

    #[tokio::test]
    async fn a_cancelled_token_stops_a_tracked_task_promptly() {
        let (tx, mut rx) = channel();
        let mut tasks = Tasks::new();
        spawn_ticks(&mut tasks, tx);

        assert!(rx.recv().await.is_some(), "ticking before cancellation");
        tasks.shutdown().await;

        // Shutdown drops the task and with it the only remaining sender, so the channel
        // closes once whatever was already queued has been drained. A timeout here would
        // mean a task outlived cancellation and is still holding a sender.
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(drained.is_ok(), "channel never closed after shutdown");
    }

    #[test]
    fn child_tokens_are_cancelled_by_the_parent() {
        let tasks = Tasks::new();
        let child = tasks.token();
        assert!(!child.is_cancelled());

        tasks.cancel.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn quit_reasons_distinguish_user_from_signal() {
        assert_ne!(
            QuitReason::User,
            QuitReason::Signal(Termination::Interrupt),
            "the exit code differs"
        );
    }

    /// Large payloads are boxed so the enum stays small — every event pays the size of
    /// the biggest variant, and pages carry a whole screen of merge requests.
    #[test]
    fn the_event_enum_stays_small() {
        let size = std::mem::size_of::<AppEvent>();
        assert!(
            size <= 64,
            "AppEvent grew to {size} bytes; box the large variants"
        );
    }
}
