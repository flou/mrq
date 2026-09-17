//! Termination signals.
//!
//! `SIGINT`, `SIGTERM` and `SIGHUP` restore the terminal and exit cleanly.
//!
//! These are handled as an async stream rather than as raw `sigaction` handlers. A real
//! signal handler may only call async-signal-safe functions, which rules out the whole
//! restoration path — it writes escape sequences and calls into crossterm. Delivering the
//! signal to the event loop instead lets shutdown run as ordinary code, so the guard's
//! `Drop` does the restoring and there is exactly one restoration path rather than two.
//!
//! The safety net for the case this cannot cover — a wedged loop that never processes the
//! signal — is the panic hook plus `Drop`, and ultimately the terminal's own reset.
//!
//! # Registration is synchronous, and happens before the terminal guard
//!
//! `tokio::signal::unix::signal` only takes effect once called — a signal that arrives
//! before it runs keeps its default disposition. Registering lazily, on the first poll of
//! a spawned task, leaves a window between the terminal guard taking over and that task
//! first being scheduled in which a SIGINT or SIGTERM kills the process outright with the
//! terminal still in raw mode on the alternate screen — the one failure this module exists
//! to prevent. [`Signals::register`] is therefore synchronous and is called before
//! `Guard::enter`, so registration either has happened or has been logged as failed before
//! there is anything on screen worth protecting.

use tokio::signal::unix::{Signal, SignalKind, signal};

/// Which signal asked the process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Interrupt,
    Terminate,
    Hangup,
}

impl Termination {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
        }
    }

    /// The conventional exit code for a signal: 128 plus the signal number.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Interrupt => 128 + 2,
            Self::Terminate => 128 + 15,
            Self::Hangup => 128 + 1,
        }
    }
}

/// Registered signal handlers, ready to be waited on.
pub struct Signals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl Signals {
    /// Install the handlers. Call this before the terminal guard takes over.
    pub fn register() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Wait for whichever of the three arrives first.
    pub async fn terminated(&mut self) -> Termination {
        tokio::select! {
            _ = self.interrupt.recv() => Termination::Interrupt,
            _ = self.terminate.recv() => Termination::Terminate,
            _ = self.hangup.recv() => Termination::Hangup,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_128_plus_signal_convention() {
        assert_eq!(Termination::Interrupt.exit_code(), 130);
        assert_eq!(Termination::Terminate.exit_code(), 143);
        assert_eq!(Termination::Hangup.exit_code(), 129);
    }

    #[test]
    fn signals_are_named_for_the_log() {
        assert_eq!(Termination::Interrupt.as_str(), "SIGINT");
        assert_eq!(Termination::Terminate.as_str(), "SIGTERM");
        assert_eq!(Termination::Hangup.as_str(), "SIGHUP");
    }

    /// The end-to-end property: a SIGTERM delivered to this process resolves the future,
    /// which is what lets shutdown run as ordinary code and restore the terminal.
    ///
    /// Registration happens synchronously, before the signal is raised, rather than
    /// inside the spawned task: that ordering is the whole point of splitting
    /// `register()` from `terminated()`, so this test only has to wait for the task to
    /// reach its first `.await`, not for the handlers to exist at all.
    #[tokio::test]
    async fn a_delivered_signal_resolves_the_future() {
        let mut signals = Signals::register().expect("signal handlers should register");
        let handle = tokio::spawn(async move { signals.terminated().await });

        // Give the spawned task a moment to reach `terminated().await` before raising.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // SAFETY-equivalent: libc::raise is not exposed, so go through the shell rather
        // than adding a libc dependency for one test.
        let pid = std::process::id();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("signal should arrive")
            .expect("task should not panic");

        assert_eq!(got, Termination::Terminate);
    }
}
