//! Application core: the event loop and the single owner of mutable state.
//!
//! One `mpsc` channel of events feeds one state owner; there are no locks over
//! application state. `app` drives `gitlab`, `ui` and `term`, and reads `config`.
//! Nothing calls back up into `app`.
//!
//! Planned contents:
//!
//! - `event`     — the `AppEvent` enum and the input/tick/fetch task wiring
//! - `state`     — per-filter tab state: snapshot, sort, drafts, scroll, selection
//! - `scheduler` — per-filter refresh timers, jitter, stagger, concurrency cap
//! - `cache`     — snapshot persistence and the warm first paint
//! - `sort`      — comparators with the mandatory stable tiebreak
//! - `filter`    — incremental search and draft visibility
//! - `diff`      — snapshot diffing and the notification trigger rules
//! - `notify`    — coalescing, focus gating and cross-restart dedup
//!
//! Rendering is redraw-on-change: key input paints immediately, and background changes
//! coalesce on the 100 ms render tick, so an idle `mrq` costs no CPU.

#[path = "loop_.rs"]
pub mod app_loop;
pub mod event;

pub mod state;

pub mod cache;
pub mod diff;
pub mod notify;
pub mod sort;

pub mod scheduler;

pub mod run;

pub mod action;
