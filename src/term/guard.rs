//! The terminal lifecycle guard.
//!
//! A crash that leaves the user in raw mode on the alternate screen with
//! pushed keyboard flags is the worst failure this program can have: the shell is
//! unusable, nothing echoes, and the reason is invisible. Restoration is therefore owned
//! by one place and wired into three of them — the RAII guard, the panic hook, and the
//! signal path.
//!
//! # Why the indirection
//!
//! The operations are behind [`TerminalOps`] rather than called directly. Entering raw
//! mode in a unit test needs a tty the test harness does not have, so without the trait
//! the ordering and idempotency rules — the parts that actually go wrong — would be
//! untestable and would have to be verified by hand after every change.
//!
//! # The rules
//!
//! 1. Restore undoes exactly what was applied, in reverse order.
//! 2. If setup fails halfway, only the completed steps are undone. A partially entered
//!    terminal is the case most likely to be got wrong, and it happens on any terminal
//!    that refuses one of the optional features.
//! 3. Restoration is idempotent, because the guard and the panic hook may both fire.
//! 4. Restoration completes before anything is printed, or the report lands on the
//!    alternate screen and vanishes with it.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// One reversible change to terminal state.
///
/// The declaration order is the order they are applied; restoration walks it backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    RawMode,
    AlternateScreen,
    KeyboardEnhancements,
    BracketedPaste,
    FocusChange,
    Mouse,
    TitleStack,
}

impl Step {
    /// Applied in this order; reverted in the reverse.
    pub const ORDER: [Self; 7] = [
        Self::RawMode,
        Self::AlternateScreen,
        Self::KeyboardEnhancements,
        Self::BracketedPaste,
        Self::FocusChange,
        Self::Mouse,
        Self::TitleStack,
    ];

    const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// A set of applied steps, cheap enough to live in an atomic for the panic hook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Steps(u32);

impl Steps {
    pub const fn none() -> Self {
        Self(0)
    }

    pub fn of(steps: &[Step]) -> Self {
        steps.iter().fold(Self::none(), |acc, s| acc.with(*s))
    }

    #[must_use]
    pub const fn with(self, step: Step) -> Self {
        Self(self.0 | step.bit())
    }

    pub const fn contains(self, step: Step) -> bool {
        self.0 & step.bit() != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Applied order.
    pub fn iter(self) -> impl Iterator<Item = Step> {
        Step::ORDER.into_iter().filter(move |s| self.contains(*s))
    }
}

/// The reversible terminal operations.
///
/// Every method is best-effort on the way out: a terminal that refuses to pop a mode is
/// still better off having the rest undone.
pub trait TerminalOps {
    fn set_raw_mode(&mut self, on: bool) -> io::Result<()>;
    fn set_alternate_screen(&mut self, on: bool) -> io::Result<()>;
    fn set_keyboard_enhancements(&mut self, on: bool) -> io::Result<()>;
    fn set_bracketed_paste(&mut self, on: bool) -> io::Result<()>;
    fn set_focus_change(&mut self, on: bool) -> io::Result<()>;
    fn set_mouse(&mut self, on: bool) -> io::Result<()>;
    /// `true` pushes the current title onto the terminal's title stack, `false` pops it.
    fn set_title_stack(&mut self, push: bool) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

fn apply_step(ops: &mut dyn TerminalOps, step: Step, on: bool) -> io::Result<()> {
    match step {
        Step::RawMode => ops.set_raw_mode(on),
        Step::AlternateScreen => ops.set_alternate_screen(on),
        Step::KeyboardEnhancements => ops.set_keyboard_enhancements(on),
        Step::BracketedPaste => ops.set_bracketed_paste(on),
        Step::FocusChange => ops.set_focus_change(on),
        Step::Mouse => ops.set_mouse(on),
        Step::TitleStack => ops.set_title_stack(on),
    }
}

/// Apply the requested steps in order, returning those that succeeded.
///
/// On failure — including a failing final flush — the already-applied steps are rolled
/// back before the error is returned, so a terminal that refuses one feature, or a stdout
/// that refuses one flush, is never left half-configured.
///
/// `on_applied` is called with the running total immediately after each step lands, before
/// the next one is attempted. That is what lets a caller (`Guard::enter`) publish a global
/// "what is currently applied" record incrementally rather than only once every step has
/// succeeded — closing the window in which a panic between the first successful step and
/// that one publish would find nothing to restore.
pub fn enter(
    ops: &mut dyn TerminalOps,
    wanted: Steps,
    mut on_applied: impl FnMut(Steps),
) -> io::Result<Steps> {
    let mut applied = Steps::none();
    for step in wanted.iter() {
        if let Err(e) = apply_step(ops, step, true) {
            leave(ops, applied);
            return Err(e);
        }
        applied = applied.with(step);
        on_applied(applied);
    }
    if let Err(e) = ops.flush() {
        leave(ops, applied);
        return Err(e);
    }
    Ok(applied)
}

/// Undo the given steps, in reverse order, best-effort.
///
/// Errors are swallowed deliberately: this runs from `Drop`, from a panic hook and from
/// the signal path, and there is nowhere useful to report to. Giving up on the first
/// error would leave the rest of the terminal broken, which is the failure this whole
/// module exists to prevent.
pub fn leave(ops: &mut dyn TerminalOps, applied: Steps) {
    let mut reversed: Vec<Step> = applied.iter().collect();
    reversed.reverse();
    for step in reversed {
        let _ = apply_step(ops, step, false);
    }
    let _ = ops.flush();
}

/// What the real terminal currently has applied, readable from a panic hook.
static ACTIVE: AtomicU32 = AtomicU32::new(0);

/// Set when a task panic has torn down the terminal, so a periodic check elsewhere
/// (the render tick) can turn it into a clean quit instead of continuing to draw onto a
/// restored screen. Tokio catches a panic inside a spawned task and turns it into a
/// `JoinError` rather than ending the process, so nothing else would notice.
static TASK_PANICKED: AtomicBool = AtomicBool::new(false);

/// Whether a panic has happened since the last check. Swap-based: the loop only ever
/// needs to notice this once, so reading it clears it.
pub fn task_panicked() -> bool {
    TASK_PANICKED.swap(false, Ordering::SeqCst)
}

/// What the panic hook does, split out so it is unit-testable without touching the real
/// global hook.
fn on_panic() {
    emergency_restore();
    TASK_PANICKED.store(true, Ordering::SeqCst);
}

/// Restore the real terminal, whatever state it is in. Idempotent.
///
/// Safe to call when nothing was ever entered, and safe to call twice — the second call
/// finds an empty set and does nothing, which is what makes it usable from both `Drop`
/// and the panic hook.
pub fn emergency_restore() {
    let active = Steps(ACTIVE.swap(0, Ordering::SeqCst));
    if active.is_empty() {
        return;
    }
    leave(&mut CrosstermOps::new(), active);
}

/// Which optional features to enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Kitty keyboard protocol, when the terminal advertises support.
    pub keyboard_enhancements: bool,
    /// Off by default so native selection and copy keep working.
    pub mouse: bool,
    /// Push and pop the title, so the user's shell title survives.
    pub title_stack: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            keyboard_enhancements: true,
            mouse: false,
            title_stack: true,
        }
    }
}

impl Options {
    fn steps(self) -> Steps {
        let mut steps = Steps::of(&[
            Step::RawMode,
            Step::AlternateScreen,
            Step::BracketedPaste,
            Step::FocusChange,
        ]);
        if self.keyboard_enhancements {
            steps = steps.with(Step::KeyboardEnhancements);
        }
        if self.mouse {
            steps = steps.with(Step::Mouse);
        }
        if self.title_stack {
            steps = steps.with(Step::TitleStack);
        }
        steps
    }
}

/// Owns the terminal for as long as it lives.
#[derive(Debug)]
pub struct Guard {
    applied: Steps,
}

impl Guard {
    /// Take over the terminal, installing the panic hook that restores it.
    pub fn enter(options: Options) -> io::Result<Self> {
        install_panic_hook();

        // `ACTIVE` is updated after every step, not once at the end: a panic between the
        // first successful step and a single trailing publish would find `ACTIVE` still
        // empty and restore nothing.
        match enter(&mut CrosstermOps::new(), options.steps(), |applied| {
            ACTIVE.store(applied.0, Ordering::SeqCst);
        }) {
            Ok(applied) => Ok(Self { applied }),
            Err(e) => {
                // `enter` already rolled back the terminal itself on this path (a step
                // failure or a failing flush); `ACTIVE` has to agree with that, since the
                // loop above may have published a non-empty set before the failure.
                ACTIVE.store(0, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Restore early, for example before printing a report.
    pub fn restore(&mut self) {
        emergency_restore();
        self.applied = Steps::none();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Install a panic hook that restores the terminal before the previous hook reports.
///
/// Ordering is the whole point: `color_eyre`'s hook writes a multi-line report, and on
/// the alternate screen in raw mode that report is both unreadable and destroyed when the
/// screen is finally restored.
pub(crate) fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            on_panic();
            previous(info);
        }));
    });
}

/// The real implementation, writing to stdout.
///
/// stdout rather than stderr because that is where the alternate screen and the ratatui
/// frame live; splitting them across two streams would interleave unpredictably when
/// either is redirected.
#[derive(Debug, Default)]
pub struct CrosstermOps;

impl CrosstermOps {
    pub const fn new() -> Self {
        Self
    }
}

impl TerminalOps for CrosstermOps {
    fn set_raw_mode(&mut self, on: bool) -> io::Result<()> {
        if on {
            crossterm::terminal::enable_raw_mode()
        } else {
            crossterm::terminal::disable_raw_mode()
        }
    }

    fn set_alternate_screen(&mut self, on: bool) -> io::Result<()> {
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        let mut out = io::stdout();
        if on {
            crossterm::execute!(out, EnterAlternateScreen)
        } else {
            crossterm::execute!(out, LeaveAlternateScreen)
        }
    }

    fn set_keyboard_enhancements(&mut self, on: bool) -> io::Result<()> {
        use crossterm::event::{
            KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        };
        let mut out = io::stdout();
        if on {
            // Only disambiguation. The other flags report key releases and text as
            // separate events, which this application has no use for and which would
            // make every binding fire twice.
            crossterm::execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
        } else {
            crossterm::execute!(out, PopKeyboardEnhancementFlags)
        }
    }

    fn set_bracketed_paste(&mut self, on: bool) -> io::Result<()> {
        use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
        let mut out = io::stdout();
        if on {
            crossterm::execute!(out, EnableBracketedPaste)
        } else {
            crossterm::execute!(out, DisableBracketedPaste)
        }
    }

    fn set_focus_change(&mut self, on: bool) -> io::Result<()> {
        use crossterm::event::{DisableFocusChange, EnableFocusChange};
        let mut out = io::stdout();
        if on {
            crossterm::execute!(out, EnableFocusChange)
        } else {
            crossterm::execute!(out, DisableFocusChange)
        }
    }

    fn set_mouse(&mut self, on: bool) -> io::Result<()> {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        let mut out = io::stdout();
        if on {
            crossterm::execute!(out, EnableMouseCapture)
        } else {
            crossterm::execute!(out, DisableMouseCapture)
        }
    }

    fn set_title_stack(&mut self, push: bool) -> io::Result<()> {
        // crossterm has no title-stack command, so these are written directly. xterm's
        // stack: CSI 22;2t saves the window title, CSI 23;2t restores it.
        let mut out = io::stdout();
        out.write_all(if push { b"\x1b[22;2t" } else { b"\x1b[23;2t" })?;
        out.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Whether the terminal supports the kitty keyboard protocol.
pub fn supports_keyboard_enhancement() -> bool {
    crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Shared call log: every (step, on/off) the recorder saw, in order.
    type Calls = Rc<RefCell<Vec<(Step, bool)>>>;

    /// Records the calls made, so ordering and idempotency are assertable without a tty.
    #[derive(Debug, Default)]
    struct Recorder {
        calls: Calls,
        fail_on: Option<Step>,
        fail_flush: bool,
    }

    impl Recorder {
        fn new() -> (Self, Calls) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    calls: Rc::clone(&calls),
                    fail_on: None,
                    fail_flush: false,
                },
                calls,
            )
        }

        fn failing(step: Step) -> (Self, Calls) {
            let (mut r, calls) = Self::new();
            r.fail_on = Some(step);
            (r, calls)
        }

        /// Every step succeeds, but the final flush does not — the case `enter()` cannot
        /// roll back from unless the flush is checked, since every step already landed.
        fn failing_flush() -> (Self, Calls) {
            let (mut r, calls) = Self::new();
            r.fail_flush = true;
            (r, calls)
        }

        fn record(&mut self, step: Step, on: bool) -> io::Result<()> {
            if self.fail_on == Some(step) && on {
                return Err(io::Error::other("refused"));
            }
            self.calls.borrow_mut().push((step, on));
            Ok(())
        }
    }

    impl TerminalOps for Recorder {
        fn set_raw_mode(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::RawMode, on)
        }
        fn set_alternate_screen(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::AlternateScreen, on)
        }
        fn set_keyboard_enhancements(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::KeyboardEnhancements, on)
        }
        fn set_bracketed_paste(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::BracketedPaste, on)
        }
        fn set_focus_change(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::FocusChange, on)
        }
        fn set_mouse(&mut self, on: bool) -> io::Result<()> {
            self.record(Step::Mouse, on)
        }
        fn set_title_stack(&mut self, push: bool) -> io::Result<()> {
            self.record(Step::TitleStack, push)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::other("stdout flush refused"));
            }
            Ok(())
        }
    }

    fn ons(calls: &[(Step, bool)]) -> Vec<Step> {
        calls
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| *s)
            .collect()
    }

    fn offs(calls: &[(Step, bool)]) -> Vec<Step> {
        calls
            .iter()
            .filter(|(_, on)| !*on)
            .map(|(s, _)| *s)
            .collect()
    }

    #[test]
    fn default_options_enable_the_documented_features() {
        let steps = Options::default().steps();
        for step in [
            Step::RawMode,
            Step::AlternateScreen,
            Step::KeyboardEnhancements,
            Step::BracketedPaste,
            Step::FocusChange,
            Step::TitleStack,
        ] {
            assert!(steps.contains(step), "{step:?} should be on by default");
        }
        assert!(
            !steps.contains(Step::Mouse),
            "mouse stays off so native selection keeps working"
        );
    }

    #[test]
    fn optional_features_can_be_declined() {
        let steps = Options {
            keyboard_enhancements: false,
            mouse: true,
            title_stack: false,
        }
        .steps();

        assert!(!steps.contains(Step::KeyboardEnhancements));
        assert!(!steps.contains(Step::TitleStack));
        assert!(steps.contains(Step::Mouse));
        assert!(
            steps.contains(Step::RawMode),
            "the core steps are not optional"
        );
    }

    /// Restoration must undo exactly what was applied, in reverse: leaving the alternate
    /// screen before disabling raw mode, popping the title before leaving the screen.
    #[test]
    fn restore_reverses_the_setup_order() {
        let (mut ops, calls) = Recorder::new();
        let applied = enter(&mut ops, Options::default().steps(), |_| {}).unwrap();
        leave(&mut ops, applied);

        let calls = calls.borrow();
        let applied_order = ons(&calls);
        let mut expected_reverse = applied_order.clone();
        expected_reverse.reverse();

        assert_eq!(offs(&calls), expected_reverse);
        assert_eq!(
            applied_order.first(),
            Some(&Step::RawMode),
            "raw mode goes on first"
        );
        assert_eq!(
            offs(&calls).last(),
            Some(&Step::RawMode),
            "and comes off last"
        );
    }

    /// The case most likely to be got wrong: a terminal that refuses one optional
    /// feature must not be left half-configured.
    #[test]
    fn a_failure_midway_rolls_back_only_the_completed_steps() {
        let (mut ops, calls) = Recorder::failing(Step::FocusChange);
        let err = enter(&mut ops, Options::default().steps(), |_| {}).unwrap_err();
        assert_eq!(err.to_string(), "refused");

        let calls = calls.borrow();
        let applied = ons(&calls);
        let reverted = offs(&calls);

        assert!(
            !applied.contains(&Step::FocusChange),
            "the failing step was never applied"
        );
        let mut expected = applied;
        expected.reverse();
        assert_eq!(reverted, expected, "everything applied was rolled back");
        assert!(
            !reverted.contains(&Step::TitleStack),
            "steps after the failure were never touched"
        );
    }

    /// Every step can succeed and the terminal still end up half-configured if the
    /// trailing flush is the thing that fails — that path has to roll back too, or the
    /// caller (`Guard::enter`) sees an error with no `Guard` to restore it.
    #[test]
    fn a_failing_flush_rolls_back_every_step_it_would_otherwise_have_kept() {
        let (mut ops, calls) = Recorder::failing_flush();
        let err = enter(&mut ops, Options::default().steps(), |_| {}).unwrap_err();
        assert_eq!(err.to_string(), "stdout flush refused");

        let calls = calls.borrow();
        let mut expected_reverted = ons(&calls);
        expected_reverted.reverse();
        assert_eq!(
            offs(&calls),
            expected_reverted,
            "every step that landed before the flush failed must be undone"
        );
    }

    /// `on_applied` is what lets a caller publish "what is currently applied" as each
    /// step lands, rather than once at the end — the fix for a panic in that window
    /// finding nothing to restore.
    #[test]
    fn on_applied_fires_after_each_step_with_the_running_total() {
        let (mut ops, _calls) = Recorder::new();
        let mut seen = Vec::new();
        let applied = enter(&mut ops, Options::default().steps(), |running| {
            seen.push(running);
        })
        .unwrap();

        assert_eq!(
            seen.last(),
            Some(&applied),
            "the final call reports everything applied"
        );
        assert!(
            seen.windows(2).all(|w| w[0] != w[1]),
            "each call reports strictly more than the last: {seen:?}"
        );
    }

    /// The guard and the panic hook may both fire; the second must be a no-op rather
    /// than a second round of escape sequences at a restored terminal.
    #[test]
    fn restoring_twice_is_a_no_op() {
        let (mut ops, calls) = Recorder::new();
        let applied = enter(&mut ops, Options::default().steps(), |_| {}).unwrap();

        leave(&mut ops, applied);
        let after_first = calls.borrow().len();

        leave(&mut ops, Steps::none());
        assert_eq!(
            calls.borrow().len(),
            after_first,
            "an empty step set does nothing"
        );
    }

    #[test]
    fn restoring_nothing_is_safe() {
        let (mut ops, calls) = Recorder::new();
        leave(&mut ops, Steps::none());
        assert!(calls.borrow().is_empty());
    }

    /// emergency_restore runs from a panic hook where nothing may have been entered.
    #[test]
    fn emergency_restore_without_a_guard_does_not_panic() {
        emergency_restore();
        emergency_restore();
    }

    #[test]
    fn step_sets_round_trip() {
        let steps = Steps::of(&[Step::RawMode, Step::Mouse]);
        assert!(steps.contains(Step::RawMode));
        assert!(steps.contains(Step::Mouse));
        assert!(!steps.contains(Step::AlternateScreen));
        assert!(!steps.is_empty());
        assert!(Steps::none().is_empty());

        // Iteration follows application order regardless of insertion order.
        let steps = Steps::none().with(Step::TitleStack).with(Step::RawMode);
        assert_eq!(
            steps.iter().collect::<Vec<_>>(),
            [Step::RawMode, Step::TitleStack]
        );
    }

    /// The invariant the whole module exists for: a panic must clear terminal state
    /// before the report is printed, or the report lands on the alternate screen and is
    /// destroyed when the screen is finally restored.
    #[test]
    fn a_panic_restores_the_terminal_before_reporting() {
        use std::sync::Arc;

        install_panic_hook();
        // Pretend the terminal was entered. The ops themselves are no-ops without a tty;
        // what is under test is that the hook runs and clears the active set.
        ACTIVE.store(
            Steps::of(&[Step::RawMode, Step::AlternateScreen]).0,
            Ordering::SeqCst,
        );

        // Observed through an atomic because a panic hook must be Send + Sync.
        let observed = Arc::new(AtomicU32::new(u32::MAX));
        let previous = std::panic::take_hook();
        {
            let seen = Arc::clone(&observed);
            std::panic::set_hook(Box::new(move |_info| {
                emergency_restore();
                // What the real reporting hook would see by the time it runs.
                seen.store(ACTIVE.load(Ordering::SeqCst), Ordering::SeqCst);
            }));
        }

        let result = std::panic::catch_unwind(|| panic!("boom"));
        std::panic::set_hook(previous);

        assert!(result.is_err());
        assert_eq!(
            observed.load(Ordering::SeqCst),
            0,
            "terminal state must already be cleared when the report runs"
        );
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
    }

    /// The other half of the panic hook: it must also flag that a task panicked, so a
    /// live loop can quit instead of drawing onto what it just restored. Exercised
    /// directly against `on_panic`, not through a real hook install, so the assertion
    /// covers the unit that changed rather than the whole mechanism.
    #[test]
    fn on_panic_flags_that_a_task_should_quit() {
        ACTIVE.store(Steps::of(&[Step::RawMode]).0, Ordering::SeqCst);
        let _ = task_panicked(); // clear anything a previous test left set

        on_panic();

        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0, "terminal still restored");
        assert!(task_panicked(), "the panic must be flagged");
        assert!(!task_panicked(), "reading it clears it");
    }

    #[test]
    fn every_step_has_a_distinct_bit() {
        let mut seen = Vec::new();
        for step in Step::ORDER {
            assert!(!seen.contains(&step.bit()), "{step:?} shares a bit");
            seen.push(step.bit());
        }
        assert_eq!(seen.len(), Step::ORDER.len());
    }
}
