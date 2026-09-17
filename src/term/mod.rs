//! Terminal integration: the parts that talk to the emulator rather than to ratatui.
//!
//! Owns every mode change and every raw escape sequence. `ui` draws through ratatui
//! and asks `term` for capabilities; it never writes an escape itself.
//!
//! Contents:
//!
//! - `guard`     — the RAII lifecycle guard: raw mode, alternate screen, keyboard
//!   flags, bracketed paste, focus events, title push/pop
//! - `caps`      — capability detection: colour depth, OSC 8, kitty keyboard, focus
//!   events, SSH
//! - `signal`    — `SIGINT`/`SIGTERM`/`SIGHUP` into a quit event, so restoration runs
//!   through the same path as a clean exit
//! - `sync`      — the DEC 2026 synchronized-output marker pair that brackets one
//!   frame, so a redraw is never shown half-painted
//! - `hyperlink` — the OSC 8 escape pair that makes a title clickable
//! - `notify`    — OSC 9 / OSC 777 / command notification backends and the bell
//! - `clipboard` — the OSC 52 copy and its native helper fallbacks
//! - `browser`   — the detached `open`/`xdg-open` launch actions
//! - `title`     — the OSC 0 screen title: format, escape, and sanitisation
//!
//! # Invariant
//!
//! A crash must never leave the user in raw mode on the alternate screen. Restoration is
//! owned by `guard` and wired into both the panic hook and the signal handlers, and it
//! runs before anything is printed. Restoration is idempotent so both paths may fire.

pub mod browser;
pub mod caps;
pub mod clipboard;
pub mod guard;
pub mod hyperlink;
pub mod notify;
pub mod signal;
pub mod sync;
pub mod title;
