//! The event loop: one owner of mutable state, one place that decides to redraw.
//!
//! The loop receives every event and folds it into state; whether it then draws
//! depends on the event and on whether anything changed.
//!
//! # Redraw-on-change
//!
//! Nothing draws unless an event since the last frame marked the state dirty. Background
//! changes — snapshots, clock ticks, resizes — wait for the render tick, so a burst
//! between ticks collapses into one frame rather than one frame each. Key input is the
//! exception: it paints immediately, because input latency is the one cost a user can
//! feel. An idle `mrq` therefore does nothing ten times a second except wake, observe
//! that nothing changed, and sleep, which is what keeps a tool left open all day off the
//! CPU.
//!
//! The distinction matters more than it looks: drawing on every tick would be simpler,
//! and would also mean a laptop never reaching a low-power state while `mrq` is open.

use crate::app::event::{AppEvent, EventReceiver, QuitReason, Tasks};
use crossterm::event::Event as TermEvent;

/// What the loop should do after handling one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit(QuitReason),
}

/// The state a loop drives.
///
/// A trait so the loop can be tested against a trivial implementation. The real state
/// lands with the tab model; what is under test here is the loop's own behaviour —
/// ordering, dirty tracking, redraw cadence and shutdown — which is where the bugs are.
pub trait Application {
    /// Fold one event into state. Return `true` if the screen needs redrawing.
    fn handle(&mut self, event: AppEvent) -> (Flow, bool);

    /// Draw the current state.
    fn draw(&mut self);
}

/// Run until something asks to quit.
///
/// Returns the reason, so `main` can choose an exit code: a signal exits 128+n by
/// convention, a user quit exits 0.
pub async fn run<A: Application>(
    app: &mut A,
    events: &mut EventReceiver,
    tasks: Tasks,
) -> QuitReason {
    let mut dirty = true;

    let reason = loop {
        let Some(event) = events.recv().await else {
            // Every sender dropped: the tasks are gone, so there is nothing left to
            // wait for and no way to observe a quit key.
            break QuitReason::User;
        };

        let is_render_tick = matches!(event, AppEvent::RenderTick);
        let is_key = matches!(&event, AppEvent::Input(TermEvent::Key(_)));
        let (flow, changed) = app.handle(event);
        dirty |= changed;

        if let Flow::Quit(reason) = flow {
            break reason;
        }

        // Keys paint immediately: input latency is the one cost a user can feel. Anything
        // else that changed state waits for a render tick, so a burst of background events
        // between ticks collapses into one frame instead of one frame each.
        if (is_render_tick || is_key) && dirty {
            app.draw();
            dirty = false;
        }
    };

    // Before the terminal guard drops, or a surviving task paints onto the restored
    // screen and corrupts the shell prompt.
    tasks.shutdown().await;
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::event::channel;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;

    #[derive(Default)]
    struct Spy {
        handled: Vec<&'static str>,
        draws: usize,
        quit_after: Option<usize>,
        dirty_on: Option<&'static str>,
    }

    fn label(event: &AppEvent) -> &'static str {
        match event {
            AppEvent::Input(_) => "input",
            AppEvent::RenderTick => "render",
            AppEvent::ClockTick => "clock",
            AppEvent::Snapshot { .. } => "snapshot",
            AppEvent::FetchFailed { .. } => "failed",
            AppEvent::FetchStarted { .. } => "started",
            AppEvent::RefreshScheduled { .. } => "scheduled",
            AppEvent::RefreshesPaused { .. } => "paused",
            AppEvent::Identified { .. } => "identified",
            AppEvent::IdentityFailed { .. } => "identity_failed",
            AppEvent::Quit(_) => "quit",
        }
    }

    impl Application for Spy {
        fn handle(&mut self, event: AppEvent) -> (Flow, bool) {
            let label = label(&event);
            self.handled.push(label);

            if let AppEvent::Quit(reason) = event {
                return (Flow::Quit(reason), false);
            }
            if self.quit_after == Some(self.handled.len()) {
                return (Flow::Quit(QuitReason::User), false);
            }

            let changed = match self.dirty_on {
                Some(wanted) => label == wanted,
                None => false,
            };
            (Flow::Continue, changed)
        }

        fn draw(&mut self) {
            self.draws += 1;
        }
    }

    async fn drive(app: &mut Spy, events: Vec<AppEvent>) -> QuitReason {
        let (tx, mut rx) = channel();
        for event in events {
            tx.send(event).unwrap();
        }
        drop(tx);
        run(app, &mut rx, Tasks::new()).await
    }

    #[tokio::test]
    async fn events_are_handled_in_order() {
        let mut app = Spy::default();
        drive(
            &mut app,
            vec![
                AppEvent::FetchStarted { filter: 0 },
                AppEvent::ClockTick,
                AppEvent::RenderTick,
            ],
        )
        .await;

        assert_eq!(app.handled, ["started", "clock", "render"]);
    }

    /// The structural property behind the idle-CPU target: an idle loop does no work
    /// per tick beyond observing that nothing changed. The target itself is measured
    /// against the real binary over a real idle period, which is mrq-0dd, not a unit
    /// test — a test process is not a tool left open all day.
    #[tokio::test]
    async fn an_idle_loop_does_not_redraw() {
        let mut app = Spy::default();
        let ticks = (0..200).map(|_| AppEvent::RenderTick).collect();
        drive(&mut app, ticks).await;

        assert_eq!(
            app.draws, 1,
            "only the initial frame; 200 further ticks changed nothing"
        );
    }

    #[tokio::test]
    async fn a_change_causes_exactly_one_redraw() {
        let mut app = Spy {
            dirty_on: Some("snapshot"),
            ..Spy::default()
        };

        drive(
            &mut app,
            vec![
                AppEvent::RenderTick, // initial frame
                AppEvent::FetchStarted { filter: 0 },
                AppEvent::RenderTick,
            ],
        )
        .await;

        assert_eq!(app.draws, 1, "FetchStarted did not mark anything dirty");
    }

    /// Several changes between two ticks collapse into one frame, rather than one frame
    /// per change.
    #[tokio::test]
    async fn bursts_between_ticks_collapse_into_one_frame() {
        let mut app = Spy {
            dirty_on: Some("clock"),
            ..Spy::default()
        };

        let mut events = vec![AppEvent::RenderTick];
        events.extend((0..50).map(|_| AppEvent::ClockTick));
        events.push(AppEvent::RenderTick);
        drive(&mut app, events).await;

        assert_eq!(app.draws, 2, "one initial frame, one for the whole burst");
    }

    /// Changes only paint on a render tick, so a producer cannot drive the frame rate.
    #[tokio::test]
    async fn changes_without_a_tick_do_not_draw() {
        let mut app = Spy {
            dirty_on: Some("clock"),
            ..Spy::default()
        };
        drive(&mut app, (0..10).map(|_| AppEvent::ClockTick).collect()).await;

        assert_eq!(app.draws, 0);
    }

    fn key() -> AppEvent {
        AppEvent::Input(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::empty(),
        )))
    }

    /// A key that changed state paints at once rather than waiting for a tick. The 100 ms
    /// cap exists to amortise background changes; input latency is what a user can feel.
    #[tokio::test]
    async fn a_changed_key_draws_immediately() {
        let mut app = Spy {
            dirty_on: Some("input"),
            ..Spy::default()
        };

        drive(
            &mut app,
            vec![key(), key(), key(), AppEvent::Quit(QuitReason::User)],
        )
        .await;

        assert_eq!(
            app.draws, 3,
            "every changed key painted, with no tick in sight"
        );
    }

    /// A key that changed nothing (an unbound key, say) draws nothing, or every stray
    /// keypress would paint an identical frame.
    #[tokio::test]
    async fn an_unchanged_key_does_not_draw() {
        let mut app = Spy::default();

        drive(&mut app, vec![key(), key(), AppEvent::RenderTick]).await;

        assert_eq!(app.draws, 1, "only the initial frame");
    }

    #[tokio::test]
    async fn a_quit_event_stops_the_loop_immediately() {
        let mut app = Spy::default();
        let reason = drive(
            &mut app,
            vec![
                AppEvent::RenderTick,
                AppEvent::Quit(QuitReason::User),
                AppEvent::RenderTick,
            ],
        )
        .await;

        assert_eq!(reason, QuitReason::User);
        assert_eq!(
            app.handled,
            ["render", "quit"],
            "events after the quit are not handled"
        );
    }

    /// The exit code differs between the two, so the reason has to survive the loop.
    #[tokio::test]
    async fn the_quit_reason_is_returned() {
        use crate::term::signal::Termination;

        let mut app = Spy::default();
        let reason = drive(
            &mut app,
            vec![AppEvent::Quit(QuitReason::Signal(Termination::Terminate))],
        )
        .await;

        assert_eq!(reason, QuitReason::Signal(Termination::Terminate));
    }

    #[tokio::test]
    async fn the_application_may_request_a_quit_from_any_event() {
        let mut app = Spy {
            quit_after: Some(2),
            ..Spy::default()
        };
        let reason = drive(&mut app, (0..5).map(|_| AppEvent::RenderTick).collect()).await;

        assert_eq!(reason, QuitReason::User);
        assert_eq!(app.handled.len(), 2);
    }

    /// If every sender is dropped the loop has nothing left to wait for; hanging there
    /// would leave the terminal in raw mode with no way out.
    #[tokio::test]
    async fn a_closed_channel_ends_the_loop() {
        let mut app = Spy::default();
        let reason = drive(&mut app, vec![]).await;
        assert_eq!(reason, QuitReason::User);
    }

    /// Shutdown is awaited inside `run`, so the guard cannot restore the terminal while
    /// a task is still painting.
    #[tokio::test]
    async fn tasks_are_shut_down_before_run_returns() {
        use crate::app::event::spawn_ticks;

        let (tx, mut rx) = channel();
        let mut tasks = Tasks::new();
        spawn_ticks(&mut tasks, tx.clone());
        tx.send(AppEvent::Quit(QuitReason::User)).unwrap();

        let mut app = Spy::default();
        tokio::time::timeout(Duration::from_secs(5), run(&mut app, &mut rx, tasks))
            .await
            .expect("run should return promptly, having awaited shutdown");
    }

    /// A resize storm cannot drive the frame rate: resizes mark state dirty but only a
    /// render tick spends that.
    #[tokio::test]
    async fn a_resize_storm_still_draws_once_per_tick() {
        let mut app = Spy {
            dirty_on: Some("input"),
            ..Spy::default()
        };

        let mut events: Vec<AppEvent> = (0..100)
            .map(|_| AppEvent::Input(crossterm::event::Event::Resize(80, 24)))
            .collect();
        events.push(AppEvent::RenderTick);
        drive(&mut app, events).await;

        assert_eq!(app.draws, 1);
    }
}
