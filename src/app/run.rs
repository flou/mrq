//! Assembly: the state owner that the event loop drives, and the startup sequence.
//!
//! Everything below this point exists in isolation and is tested in isolation. This is
//! the only module that knows they belong together, and the only one that can be wrong
//! about how.
//!
//! # Startup order
//!
//! Config and the identity probe both run *before* the terminal guard. Both can fail
//! fatally, and a failure reported onto the alternate screen is destroyed the moment the
//! screen is restored — the user sees the program exit with no explanation.

use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::app::action::{self, Effect, KeyOutcome, Mode, ViewState};
use crate::app::app_loop::{Application, Flow};
use crate::app::event::{AppEvent, EventSender, QuitReason, Tasks, channel};
use crate::app::scheduler::{RefreshHandle, RefreshRequest};
use crate::app::state::{Tab, Tabs};
use crate::app::{event as app_event, scheduler};
use crate::config::keymap::Keymap;
use crate::config::load::Loaded;
use crate::config::schema::{Column, Config};
use crate::error::{Error, Result};
use crate::gitlab::client::Client;
use crate::logging;
use crate::term::caps::{Capabilities, TermEnv};
use crate::term::guard::{self, Guard};
use crate::ui;
use crate::ui::statusbar::{self, Status};
use crate::ui::theme::Theme;

type Backend = CrosstermBackend<std::io::Stdout>;

/// The application state the loop owns.
pub struct App {
    view: ViewState,
    config: Config,
    keymap: Keymap,
    refresh: RefreshHandle,
    terminal: Terminal<Backend>,
    /// Whether the terminal currently has focus, for the notification gating.
    focused: bool,
    /// The same fact, shared with the refresh workers for `pause_when_unfocused`.
    /// This loop is the only writer.
    focus_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether a runtime 401/403 has stopped every filter, for the status bar.
    refreshes_paused: bool,
    /// The same fact, shared with the refresh workers. This loop is the only writer.
    pause_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The resolved notification mechanism for this session.
    notify_backend: crate::term::notify::Backend,
    /// Coalescing, focus gating and cross-restart dedup.
    notifier: crate::app::notify::Notifier,
    /// The clipboard helpers detected at startup, for the OSC 52 fallback chain.
    clipboard_helpers: crate::term::clipboard::Helpers,
    /// The browser launcher helpers detected at startup.
    browser_helpers: crate::term::browser::Helpers,
    /// Whether the terminal advertises OSC 8.
    hyperlinks: bool,
    /// When the session began, as the spinner's phase reference.
    started: Instant,
    quit: bool,
}

/// How long a spinner frame is shown. Fast enough to read as motion, slow enough that
/// the render tick can keep up with it.
const SPINNER_FRAME: Duration = Duration::from_millis(100);

impl App {
    /// Gather what the status bar needs.
    ///
    /// `dropped` is left empty and filled in during the draw, once the table's width —
    /// and therefore which columns survived — is known.
    fn status(
        &self,
        rows: &[&crate::gitlab::model::MergeRequest],
        selected: Option<usize>,
        tab: &Tab,
    ) -> Status {
        let now = Instant::now();

        Status {
            position: selected.map(|index| index + 1),
            count: rows.len(),
            sort_column: tab.sort_column,
            sort_order: tab.sort_order,
            show_drafts: tab.show_drafts(),
            wide: self.view.wide,
            refresh: statusbar::refresh_of(tab, now),
            search: self.view.search_text().map(str::to_owned),
            degraded: tab.fragment.lost(),
            dropped: Vec::new(),
            truncated: tab.truncated,
            auth_paused: self.refreshes_paused,
            flash: self.view.live_flash(now).map(str::to_owned),
            // Driven by the wall clock rather than a draw counter, so the spinner turns
            // at the same rate however often the frame happens to be redrawn.
            spinner: (now.saturating_duration_since(self.started).as_millis()
                / SPINNER_FRAME.as_millis()) as usize,
        }
    }

    /// Record a focus change for both readers: this loop's notification gating and
    /// the workers' pause.
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.focus_flag
            .store(focused, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record an auth pause for both readers: this loop's status bar and every
    /// worker's timer.
    ///
    /// The sole writer, so the view copy and the shared flag can never disagree — a
    /// caller clearing the pause before requesting a refresh relies on that, since a
    /// worker only ever bypasses the flag on a manual wake.
    fn set_paused(&mut self, paused: bool) {
        self.refreshes_paused = paused;
        self.pause_flag
            .store(paused, std::sync::atomic::Ordering::Relaxed);
    }

    /// The notification triggers for one filter's incoming snapshot.
    ///
    /// Runs against the tab's *current* rows, so it has to be called before they are
    /// replaced. A tab that has not fetched this session — a cold start, or a warm one
    /// off the cache — has no baseline at all rather than an empty one: an empty previous
    /// snapshot would read every row as an arrival and fire one notification per merge
    /// request at startup.
    fn diff_for(
        &self,
        filter: app_event::FilterId,
        incoming: &[crate::gitlab::model::MergeRequest],
    ) -> Vec<crate::app::diff::Event> {
        use crate::app::diff::{baseline_for, diff};

        let Some(tab) = self.view.tabs.get(filter) else {
            return Vec::new();
        };

        // The per-filter override wins over the global default, which is what makes a
        // noisy filter's notifications turn-off-able without silencing every other tab.
        let notify_enabled = self
            .config
            .filters
            .get(filter)
            .and_then(|f| f.notify)
            .unwrap_or(self.config.notifications.enabled);
        if !notify_enabled {
            return Vec::new();
        }

        diff(
            filter,
            &tab.name,
            baseline_for(tab),
            incoming,
            &self.config.notifications,
        )
    }

    /// Deliver one notification through the resolved backend.
    ///
    /// The escape goes out in a single `write_all` between frames. Split across writes,
    /// or emitted mid-draw, the bytes would interleave with ratatui's own output and the
    /// user would see fragments of an escape sequence printed into the table.
    fn notify(&self, notification: &app_event::Notification) {
        let delivery = self.notify_backend.deliver(
            &notification.title,
            &notification.body,
            self.config.notifications.bell,
        );

        if !delivery.escape.is_empty() {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            // A failed write is not worth tearing the session down for: the status bar
            // flash has already told the user, and the next one may well succeed.
            if let Err(error) = out
                .write_all(delivery.escape.as_bytes())
                .and_then(|()| out.flush())
            {
                tracing::warn!(%error, "could not write the notification escape");
            }
        }

        if let Some(command) = delivery.spawn {
            // Reaped by a task rather than left to drop: an unwaited child is a zombie
            // for the rest of the session, and a notify command that always fails is
            // worth one log line rather than silence.
            tokio::spawn(async move {
                let result = tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&command)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await;

                match result {
                    Ok(out) if !out.status.success() => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        tracing::warn!(%command, status = ?out.status.code(), stderr = %stderr.trim(), "notification command failed");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%command, %error, "could not run the notification command")
                    }
                }
            });
        }
    }

    /// Perform an effect the dispatcher asked for.
    ///
    /// Everything that reaches outside the process happens here, so `action::dispatch`
    /// stays a pure function and every action stays testable.
    fn perform(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Quit => self.quit = true,
            Effect::RefreshAll => {
                // Cleared before the request, not after: a worker only bypasses the
                // pause on a manual wake, so the flag must already be down when the
                // request reaches it.
                self.set_paused(false);
                self.refresh.request(RefreshRequest::All);
            }
            Effect::RefreshActive => {
                self.set_paused(false);
                self.refresh
                    .request(RefreshRequest::One(self.view.tabs.active_index()));
            }
            // The open half lands in `open`; the copy half is implemented in `copy`.
            Effect::Open(url) => self.open(&url),
            Effect::Copy(text) => self.copy(&text),
        }
    }

    /// Open `url` in the browser, reporting failure in the status bar.
    fn open(&mut self, url: &str) {
        use crate::term::browser::{Launch, plan, run};

        let launch = plan(&self.config.browser, &self.browser_helpers);
        if let Launch::Unavailable = launch {
            self.view
                .flash("open failed: no browser launcher on this host");
            return;
        }

        match run(url, &launch) {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, "browser launch failed");
                self.view.flash(format!("open failed: {error}"));
            }
        }
    }

    /// Put `text` on the clipboard: OSC 52 first, a native helper when the payload is
    /// too large for the escape. The dispatcher already flashed "copied" optimistically;
    /// only a failure says anything here, overwriting it.
    fn copy(&mut self, text: &str) {
        use crate::term::clipboard::{Copy, plan, run_native};

        let failure = match plan(text, &self.clipboard_helpers) {
            Copy::Osc52(escape) => {
                // The same single-write discipline as a notification escape: split
                // across writes, the bytes would interleave with a ratatui frame.
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                match out.write_all(escape.as_bytes()).and_then(|()| out.flush()) {
                    Ok(()) => None,
                    Err(error) => {
                        tracing::warn!(%error, "could not write the clipboard escape");
                        Some("the clipboard escape could not be written".to_owned())
                    }
                }
            }
            Copy::Native(command) => match run_native(&command, text) {
                Ok(()) => None,
                Err(error) => {
                    tracing::warn!(%error, "clipboard helper failed");
                    Some(error.to_string())
                }
            },
            Copy::Unavailable => Some("no clipboard backend on this host".to_owned()),
        };

        if let Some(failure) = failure {
            self.view.flash(format!("copy failed: {failure}"));
        }
    }

    /// Push the active tab's name and counts into the terminal title, when enabled.
    ///
    /// Only two things change what the title says — switching tabs, and a refresh of the
    /// tab on screen landing — and both call here before the frame that shows them.
    fn set_terminal_title(&mut self) {
        if !self.config.ui.set_terminal_title {
            return;
        }
        let index = self.view.tabs.active_index();
        let Some(tab) = self.view.tabs.get(index) else {
            return;
        };
        let text = crate::term::title::compose(
            &tab.name,
            self.view.count_of(index),
            self.view.new_count_of(index),
        );

        // The same single-write discipline as the clipboard and notification escapes.
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        if let Err(error) = out
            .write_all(crate::term::title::escape(&text).as_bytes())
            .and_then(|()| out.flush())
        {
            tracing::warn!(%error, "could not write the terminal title");
        }
    }

    /// How many table body rows the current terminal shows. The fallback matches the
    /// half-page assumption the action layer makes before the first draw.
    fn table_viewport(&self) -> usize {
        match self.terminal.size() {
            Ok(size) => {
                let table_area = ui::layout::compute(size.into()).table_inner();
                ui::table::visible_row_count(table_area)
            }
            Err(error) => {
                tracing::warn!(%error, "terminal size unavailable; assuming a half-page viewport");
                action::HALF_PAGE_VIEWPORT
            }
        }
    }

    /// Record the window every scroll decision uses, the action layer's included: it
    /// cannot ask the terminal itself, and a stale window scrolls the table under a
    /// cursor that has not reached the edge of the real one.
    fn sync_viewport(&mut self) {
        self.view.viewport = self.table_viewport();
    }

    /// A mouse event inside the table, when `[ui].mouse` is on.
    ///
    /// Wheel scrolls the cursor; a click selects the row under it, and a click on the
    /// title cell also opens that merge request. Everything else — hover, drag, the
    /// other buttons — changes nothing.
    fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        match event.kind {
            MouseEventKind::ScrollUp => action::mouse_wheel(&mut self.view, true),
            MouseEventKind::ScrollDown => action::mouse_wheel(&mut self.view, false),
            MouseEventKind::Up(MouseButton::Left) => {
                // One terminal size query for both checks below, rather than one each.
                let Some(area) = self.table_area() else {
                    return false;
                };
                let Some(index) = self.row_at(area, event.row) else {
                    return false;
                };
                let on_title = self.title_at(area, event.column);
                let selected = action::mouse_select(&mut self.view, index);
                if on_title && let Some(effect) = action::mouse_open(&self.view, index) {
                    self.perform(effect);
                }
                selected
            }
            _ => false,
        }
    }

    /// The table's inner area on the current terminal size, if one is known.
    fn table_area(&self) -> Option<Rect> {
        let size = self.terminal.size().ok()?;
        Some(ui::layout::compute(size.into()).table_inner())
    }

    /// The index into the table's visible rows that a screen row falls on, folding the
    /// current scroll offset in. Bounds are re-checked against the actual list by the
    /// action layer, because the window may run past the end of a short list.
    fn row_at(&self, area: Rect, row: u16) -> Option<usize> {
        if row <= area.top() || row >= area.bottom() {
            return None;
        }
        let offset = (row - area.top() - 1) as usize;
        Some(self.view.tabs.active()?.scroll + offset)
    }

    /// The columns to draw this frame: `[ui].columns`, minus whichever are wide-only
    /// while wide mode is off.
    fn visible_columns(&self) -> Vec<Column> {
        ui::columns::for_wide_mode(
            &self.config.ui.columns,
            &self.config.ui.wide_columns,
            self.view.wide,
        )
    }

    /// Whether a screen column falls inside the title column of the current layout.
    fn title_at(&self, area: Rect, column: u16) -> bool {
        let allocation = ui::table::allocate(
            &self.visible_columns(),
            area.width,
            &self.view.visible_rows(),
        );
        let (Some(x), Some(width)) = (
            ui::table::column_x(&allocation, area, Column::Title),
            allocation.width_of(Column::Title),
        ) else {
            return false;
        };
        column >= x && column < x + width
    }
}

impl Application for App {
    fn handle(&mut self, event: AppEvent) -> (Flow, bool) {
        let changed = match event {
            AppEvent::Input(TermEvent::Key(key)) => {
                let filter_before = self.view.tabs.active_index();
                let changed = match action::handle_key(&mut self.view, &self.keymap, key) {
                    KeyOutcome::Ignored => false,
                    KeyOutcome::Handled { redraw } => redraw,
                    KeyOutcome::Action { redraw, effect } => {
                        self.perform(effect);
                        redraw
                    }
                };
                // A filter switch changes what the title names, whatever route took it
                // there: next/previous, the number keys, or the filter switcher popup.
                if self.view.tabs.active_index() != filter_before {
                    self.set_terminal_title();
                }
                changed
            }
            AppEvent::Input(TermEvent::Resize(..)) => {
                // A resize does not draw until the next render tick; a key pressed in
                // that gap must not scroll against the old window.
                self.sync_viewport();
                true
            }
            AppEvent::Input(TermEvent::Paste(text)) => {
                match action::handle_paste(&mut self.view, &text) {
                    KeyOutcome::Ignored => false,
                    KeyOutcome::Handled { redraw } => redraw,
                    // `handle_paste` never dispatches; the arm exists for exhaustion.
                    KeyOutcome::Action { .. } => false,
                }
            }
            AppEvent::Input(TermEvent::Mouse(event)) => self.handle_mouse(event),
            AppEvent::Input(TermEvent::FocusGained) => {
                self.set_focus(true);
                // Refresh on regaining focus, if stale. Requesting is all this does —
                // the resulting FetchStarted is what redraws. Never while auth-paused:
                // alt-tabbing must not re-send a credential the instance already refused.
                for filter in scheduler::filters_to_refresh_on_focus(
                    &self.view.tabs,
                    &self.config.refresh,
                    Instant::now(),
                    self.refreshes_paused,
                ) {
                    self.refresh.request(RefreshRequest::One(filter));
                }
                false
            }
            AppEvent::Input(TermEvent::FocusLost) => {
                self.set_focus(false);
                false
            }

            AppEvent::FetchStarted { filter } => {
                if let Some(tab) = self.view.tabs.get_mut(filter) {
                    tab.begin_fetch();
                }
                true
            }
            AppEvent::Snapshot { filter, snapshot } => {
                // A successful fetch is proof the credential works, from whichever
                // filter it came from — the whole point of pausing every filter on one
                // 401 is that a stuck pause would be worse than the rare double-check.
                self.set_paused(false);
                // Diffed before the snapshot is applied: afterwards the rows it has to be
                // compared against are gone.
                let events = self.diff_for(filter, &snapshot.merge_requests);
                self.view.apply_snapshot(filter, *snapshot, Instant::now());

                for notification in
                    self.notifier
                        .process(&events, self.focused, jiff::Timestamp::now())
                {
                    self.notify(&notification);
                }
                // A refresh of the tab on screen is the other thing that changes the
                // title's numbers; a background tab's landing cannot.
                if filter == self.view.tabs.active_index() {
                    self.set_terminal_title();
                }
                true
            }
            AppEvent::RefreshScheduled { filter, due } => {
                if let Some(tab) = self.view.tabs.get_mut(filter) {
                    tab.next_refresh = Some(due);
                }
                true
            }
            AppEvent::FetchFailed { filter, error } => {
                if let Some(tab) = self.view.tabs.get_mut(filter) {
                    tab.apply_error(&error);
                }
                true
            }
            AppEvent::RefreshesPaused { filter } => {
                tracing::warn!(filter, "runtime auth failure: pausing every refresh");
                self.set_paused(true);
                // A deadline the worker will not honour is worse than none — the status
                // bar segment says why, and a countdown to nothing would keep lying.
                for tab in self.view.tabs.iter_mut() {
                    tab.next_refresh = None;
                }
                true
            }

            // Relative times are recomputed on draw, so a clock tick is a change.
            AppEvent::ClockTick => true,
            // Idle, a render tick changes nothing. While a fetch is in flight it is what
            // turns the spinner, and ratatui writes only the cells that differ.
            AppEvent::RenderTick => self.view.tabs.iter().any(|t| t.state.is_fetching()),
            AppEvent::Quit(reason) => return (Flow::Quit(reason), false),
        };

        if self.quit {
            return (Flow::Quit(QuitReason::User), changed);
        }
        (Flow::Continue, changed)
    }

    fn draw(&mut self) {
        // Refreshed before anything else, so the action layer's next scroll decision
        // sees this frame's real window rather than last frame's.
        self.sync_viewport();

        let Some(tab) = self.view.tabs.active() else {
            return;
        };

        // The rows are borrowed, not cloned, so `rows` holds a shared borrow of the same
        // tab data `active_mut` below needs exclusively — its last use has to come
        // first.
        let rows = self.view.visible_rows();
        let counts: Vec<usize> = (0..self.view.tabs.len())
            .map(|index| self.view.count_of(index))
            .collect();
        let selected = tab
            .selected_id()
            .and_then(|id| rows.iter().position(|mr| mr.id == id));
        let row_count = rows.len();

        // Reconcile the scroll offset against this frame's window: something other than
        // navigation may have moved it out of range since the last frame — a resize, a
        // shrunken row set from a search or draft toggle, or a snapshot that moved the
        // selection.
        let viewport = self.view.viewport;
        if let Some(tab) = self.view.tabs.active_mut() {
            tab.scroll =
                action::scroll_to_keep(tab.scroll, selected.unwrap_or(0), row_count, viewport);
        }

        let Some(tab) = self.view.tabs.active() else {
            return;
        };
        // Asked for again rather than held across the mutation above: the sort itself
        // is cheap here — it moves references, not the 488-byte rows the clone-based
        // version used to sort — and the ordering cannot have changed, since nothing
        // that just happened touches sort keys or the filter.
        let rows = self.view.visible_rows();
        let columns = self.visible_columns();
        let scene = ui::Scene {
            view: &self.view,
            tab,
            rows: &rows,
            counts: &counts,
            status: self.status(&rows, selected, tab),
            keymap: &self.keymap,
            theme: &self.view.theme,
            columns: &columns,
            now: jiff::Timestamp::now(),
            hyperlinks: self.hyperlinks,
        };

        // The scene borrows `self`; `Terminal::draw` only takes `self.terminal`, and
        // field-precise capture keeps the two apart. Cloning instead would deep-copy
        // every merge request in every tab, once per frame.
        //
        // A draw failure is not recoverable here and not worth tearing the session down
        // for either: the next tick tries again, and the guard still restores on exit.
        let _ = crate::term::sync::frame(|| self.terminal.draw(|frame| ui::render(frame, &scene)));
    }
}

/// Start the TUI and run until the user or a signal stops it.
pub async fn run(loaded: Loaded, log: logging::LogBuffer) -> Result<QuitReason> {
    let config = loaded.config;

    let keymap = crate::config::keymap::resolve(&config.keys)?;
    // Before the guard for the same reason as everything else here: an unknown skin name
    // has to be reported to a terminal that is still showing the user's shell.
    crate::ui::theme::check(&config.skin)?;

    // Before the guard: a fatal failure here must reach a terminal that is still
    // showing the user's shell.
    let token = crate::config::token::resolve(
        &config.gitlab,
        &crate::config::token::TokenEnv::from_process(),
        loaded.paths.config_file(),
    )?;
    for warning in &token.warnings {
        tracing::warn!(%warning, "token");
    }

    let client = Client::new(&config.gitlab, token.token)?;
    let identity = match crate::gitlab::probe::identify_and_log(&client).await {
        Ok(identity) => identity,
        Err(error) => {
            // The invariant that makes plain propagation correct here: the recovery
            // taxonomy already agrees every failure this probe can produce is fatal
            // before the alternate screen exists.
            debug_assert!(crate::gitlab::probe::is_fatal(&error));
            return Err(error);
        }
    };

    let caps = Capabilities::detect(&TermEnv::from_process());
    let mut tabs = Tabs::new(
        &config.filters,
        config.sort,
        config.ui.show_drafts,
        loaded.initial_filter.as_deref(),
    );

    // The warm start. Local disk and before the guard, so the very first
    // frame the user sees already has rows in it instead of an empty table.
    //
    // A cache directory that cannot be created disables caching rather than failing: a
    // warm start is an optimisation, and refusing to run without one would be absurd.
    let cache_dir = match loaded.paths.ensure_cache_dir() {
        Ok(dir) => Some(dir.to_path_buf()),
        Err(error) => {
            tracing::warn!(%error, "no cache directory; starting cold and not persisting");
            None
        }
    };
    if let Some(dir) = &cache_dir {
        let warmed = crate::app::cache::warm(
            &mut tabs,
            &config.filters,
            dir,
            &identity.username,
            jiff::Timestamp::now(),
        );
        tracing::info!(warmed, filters = config.filters.len(), "warm start");
    }

    // Registered synchronously, here, rather than on first poll of the spawned task
    // (`spawn_signals`, below): `signal()` only takes effect once called, so a signal that
    // arrived before that first poll would keep its default disposition and kill the
    // process outright with the terminal already in raw mode. A registration failure
    // means graceful shutdown by signal is unavailable for the session — worth a log line,
    // not worth refusing to start.
    let signals = match crate::term::signal::Signals::register() {
        Ok(signals) => Some(signals),
        Err(error) => {
            tracing::warn!(%error, "could not register signal handlers; SIGINT/SIGTERM/SIGHUP will use their default disposition");
            None
        }
    };

    // Before the probe below: `supports_keyboard_enhancement` itself enables raw mode and
    // blocks on a `/dev/tty` read, and a panic in that window must already have something
    // to restore the terminal with. `Guard::enter` installs the same hook again, which is
    // a no-op the second time.
    guard::install_panic_hook();

    // Off the async runtime: this is a synchronous, timeout-bound blocking read, and
    // `caps.rs`'s own doctrine is that a terminal query's timeout is a visible startup
    // stall — this is the one capability still detected that way rather than from `$TERM`.
    let keyboard_enhancements = tokio::task::spawn_blocking(guard::supports_keyboard_enhancement)
        .await
        .unwrap_or(false);

    let guard = Guard::enter(guard::Options {
        keyboard_enhancements,
        mouse: config.ui.mouse,
        title_stack: config.ui.set_terminal_title,
    })
    .map_err(|e| Error::Other(format!("could not take over the terminal: {e}")))?;

    let theme = Theme::resolve(&config.skin, config.ui.ascii, &caps);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| Error::Other(format!("could not initialise the terminal: {e}")))?;

    let (events, mut receiver) = channel();
    let mut tasks = Tasks::new();
    app_event::spawn_input(&mut tasks, events.clone());
    app_event::spawn_ticks(&mut tasks, events.clone());
    app_event::spawn_signals(&mut tasks, events.clone(), signals);

    // Starts focused, and stays that way on a terminal that does not report focus.
    // An undetected terminal must be treated as always focused rather than silently
    // suppressing things the user asked for.
    let focus_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Starts unpaused: the identity probe already ruled out a startup auth failure.
    let pause_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !caps.focus_events
        && (config.refresh.refresh_on_focus || config.refresh.pause_when_unfocused)
    {
        tracing::info!(
            "this terminal does not report focus events; refresh.refresh_on_focus and \
             refresh.pause_when_unfocused have no effect"
        );
    }

    // Resolved once and logged once, because "why did I not get a notification" is
    // otherwise unanswerable without a packet capture.
    let notify_backend = crate::term::notify::resolve(
        &config.notifications,
        &caps,
        crate::term::notify::Helpers::detect(),
    );
    tracing::info!(
        backend = %notify_backend.describe(),
        bell = config.notifications.bell,
        over_ssh = caps.over_ssh,
        "notifications"
    );

    // Which native helper a copy can fall back to. Detected once and logged once: the
    // log is the trail for "why did my copy go through OSC 52 / a helper / nowhere".
    let clipboard_helpers = crate::term::clipboard::Helpers::detect();
    tracing::info!(
        pbcopy = clipboard_helpers.pbcopy,
        wl_copy = clipboard_helpers.wl_copy,
        xclip = clipboard_helpers.xclip,
        "clipboard"
    );

    let browser_helpers = crate::term::browser::Helpers::detect();
    tracing::info!(
        open = browser_helpers.open,
        xdg_open = browser_helpers.xdg_open,
        "browser launchers"
    );

    // The dedup store lives in the state dir; a session without one still dedups within
    // itself and simply forgets on exit.
    let seen = match loaded.paths.ensure_state_dir() {
        Ok(dir) => crate::app::notify::Seen::load(dir, jiff::Timestamp::now()),
        Err(error) => {
            tracing::warn!(%error, "no state directory; notifications may repeat after a restart");
            crate::app::notify::Seen::ephemeral()
        }
    };
    let notifier = crate::app::notify::Notifier::new(
        crate::app::notify::Gate {
            only_when_unfocused: config.notifications.only_when_unfocused,
            focus_events: caps.focus_events,
        },
        seen,
    );
    if notifier.ignores_focus_setting() {
        tracing::info!(
            "this terminal does not report focus events; notifications.only_when_unfocused \
             is ignored rather than suppressing everything"
        );
    }
    tracing::info!(
        remembered = notifier.seen_count(),
        "notification dedup store"
    );

    let refresh = scheduler::spawn(
        &mut tasks,
        events.clone(),
        client,
        &config,
        identity.username.clone(),
        cache_dir,
        scheduler::Flags {
            focused: std::sync::Arc::clone(&focus_flag),
            auth_paused: std::sync::Arc::clone(&pause_flag),
        },
    );

    let mut view = ViewState {
        tabs,
        mode: Mode::Normal,
        wide: false,
        theme,
        drafts_last: config.sort.drafts_last,
        flash: None,
        log,
        viewport: action::HALF_PAGE_VIEWPORT,
    };
    view.select_initial_rows();

    let mut app = App {
        view,
        config,
        keymap,
        refresh,
        terminal,
        focused: true,
        focus_flag,
        refreshes_paused: false,
        pause_flag,
        notify_backend,
        notifier,
        clipboard_helpers,
        browser_helpers,
        hyperlinks: caps.hyperlinks,
        started: Instant::now(),
        quit: false,
    };
    // A key queued at startup could be handled before the first draw; give it the real
    // window rather than the fallback.
    app.sync_viewport();

    let reason = crate::app::app_loop::run(&mut app, &mut receiver, tasks).await;

    // Explicit rather than relying on the drop order at the end of the function: the
    // terminal must be restored before anything is printed about why we exited.
    drop(app);
    drop(guard);

    Ok(reason)
}

/// Keeps the sender type referenced for readers of the signature above.
const _: fn() -> (EventSender, crate::app::event::EventReceiver) = channel;
