//! When each filter refreshes.
//!
//! One task per filter, each on its own jittered timer, all of them
//! polling — including background tabs, because the notification rules compare
//! against a previous snapshot and a tab that never fetches never has one.
//!
//! # Why the timing is not simply `interval_secs`
//!
//! Starting every filter's timer at once means every refresh cycle sends N requests in
//! the same millisecond, forever. With a handful of `mrq` users on one instance that is
//! a synchronised spike against a shared service. Each filter therefore starts at a
//! staggered offset and adds a random jitter to every cycle, so the load spreads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rand::{RngExt, SeedableRng, rngs::StdRng};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::app::event::{AppEvent, EventSender, FilterId, Tasks};
use crate::config::schema::{Config, Refresh};
use crate::error::{Phase, Recovery};
use crate::gitlab::client::Client;
use crate::gitlab::error::Backoff;
use crate::gitlab::fetch::{self, Degradation};

/// A request for a filter to refresh now, sent by `ctrl-r` or a focus gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRequest {
    /// Every filter (`ctrl-r`).
    All,
    /// One filter.
    One(FilterId),
}

/// The handle the application uses to ask for refreshes.
#[derive(Debug, Clone)]
pub struct RefreshHandle {
    senders: Vec<mpsc::Sender<()>>,
}

impl RefreshHandle {
    /// Ask one or all filters to refresh immediately.
    ///
    /// A filter already fetching is not asked twice: the channel has capacity one, so a
    /// second request while one is pending is dropped rather than queued. That is the
    /// coalescing that avoids a pile-up — holding `ctrl-r` should not queue fifty
    /// refreshes to run back to back.
    pub fn request(&self, what: RefreshRequest) {
        match what {
            RefreshRequest::All => {
                for sender in &self.senders {
                    let _ = sender.try_send(());
                }
            }
            RefreshRequest::One(filter) => {
                if let Some(sender) = self.senders.get(filter) {
                    let _ = sender.try_send(());
                }
            }
        }
    }

    #[cfg(test)]
    const fn len(&self) -> usize {
        self.senders.len()
    }
}

/// The delay before a filter's first fetch.
///
/// Spread across the interval so N filters do not all fire together at startup. Capped
/// so the last filter in a long list still appears promptly — a user watching an empty
/// tab does not care that the load is nicely distributed.
pub fn startup_stagger(index: usize, count: usize, interval: Duration) -> Duration {
    if count <= 1 {
        return Duration::ZERO;
    }
    const MAX_STAGGER: Duration = Duration::from_secs(2);

    let share = interval / count.max(1) as u32;
    let step = share.min(MAX_STAGGER);
    step * index as u32
}

/// Which filters to refresh when the terminal regains focus.
///
/// Only the stale ones, and only when `refresh_on_focus` is set: refetching every filter
/// on every alt-tab would turn a window manager into a load generator against a shared
/// instance. A filter already fetching is skipped too — its result is seconds away, and
/// asking again would queue a second fetch behind the one in flight. While refreshes are
/// paused for a refused credential, a focus gain must not re-send it — that is the
/// alt-tab-as-ban-generator case the auth-pause behavior exists to avoid.
pub fn filters_to_refresh_on_focus(
    tabs: &crate::app::state::Tabs,
    refresh: &Refresh,
    now: std::time::Instant,
    auth_paused: bool,
) -> Vec<FilterId> {
    if !refresh.refresh_on_focus || auth_paused {
        return Vec::new();
    }
    let interval = Duration::from_secs(refresh.interval_secs);

    tabs.iter()
        .filter(|tab| !tab.state.is_fetching() && tab.is_stale(now, interval))
        .map(|tab| tab.index)
        .collect()
}

/// A seed that differs per filter and per process start.
fn jitter_seed(filter: FilterId) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (filter as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// The wait before the next scheduled refresh.
pub fn next_delay(refresh: &Refresh, rng: &mut impl RngExt) -> Duration {
    let base = Duration::from_secs(refresh.interval_secs);
    if refresh.jitter_secs == 0 {
        return base;
    }
    let jitter = rng.random_range(0..=refresh.jitter_secs);
    base + Duration::from_secs(jitter)
}

/// Facts the event loop shares with every worker. The loop is the only writer of both.
#[derive(Debug, Clone)]
pub struct Flags {
    /// Whether the terminal currently has focus, for `pause_when_unfocused`.
    ///
    /// Starts `true` and stays there on terminals that do not report focus, so an
    /// undetected terminal never silently stops refreshing.
    pub focused: Arc<AtomicBool>,
    /// A runtime 401/403 refused the credential.
    pub auth_paused: Arc<AtomicBool>,
}

/// Spawn one fetch task per configured filter.
///
/// Returns the handle used to request immediate refreshes.
pub fn spawn(
    tasks: &mut Tasks,
    events: EventSender,
    client: Client,
    config: &Config,
    current_user: String,
    cache_dir: Option<std::path::PathBuf>,
    flags: Flags,
) -> RefreshHandle {
    // Shared across every filter, so N tabs cannot open N simultaneous connections to an
    // instance that is also serving everyone else.
    let permits = Arc::new(Semaphore::new(config.gitlab.max_concurrent_requests.max(1)));
    let mut senders = Vec::with_capacity(config.filters.len());

    for (index, filter) in config.filters.iter().enumerate() {
        // Capacity one: a pending request is enough, and a second is the same request.
        let (tx, rx) = mpsc::channel(1);
        senders.push(tx);

        let worker = Worker {
            id: index,
            filter: filter.clone(),
            refresh: config.refresh.clone(),
            instance_url: config.gitlab.url.clone(),
            current_user: current_user.clone(),
            client: client.clone(),
            events: events.clone(),
            permits: Arc::clone(&permits),
            cache_dir: cache_dir.clone(),
            flags: flags.clone(),
            cancel: tasks.token(),
            stagger: startup_stagger(
                index,
                config.filters.len(),
                Duration::from_secs(config.refresh.interval_secs),
            ),
        };

        tasks.track(tokio::spawn(worker.run(rx)));
    }

    RefreshHandle { senders }
}

struct Worker {
    id: FilterId,
    filter: crate::config::schema::Filter,
    refresh: Refresh,
    instance_url: String,
    current_user: String,
    client: Client,
    events: EventSender,
    permits: Arc<Semaphore>,
    /// Where to persist each successful fetch. `None` disables caching,
    /// which is what happens when the directory could not be created — a warm start is
    /// an optimisation and losing it is not worth refusing to run.
    cache_dir: Option<std::path::PathBuf>,
    /// Shared with the event loop, which is the only writer.
    flags: Flags,
    cancel: CancellationToken,
    stagger: Duration,
}

/// Why the worker woke up, which decides whether a pause applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// The first pass. Skipped only by an auth pause: firing a doomed request against a
    /// credential already refused gains nothing, but `pause_when_unfocused` must not
    /// stop the fetch that puts rows on screen for the first time.
    Startup,
    /// The refresh timer elapsed. Can be suppressed by either pause.
    Timer,
    /// The user asked, via `ctrl-r` or a focus gain. Never paused — they are waiting.
    Manual,
}

impl Worker {
    async fn run(self, mut manual: mpsc::Receiver<()>) {
        let mut backoff = Backoff::new(Duration::from_secs(self.refresh.interval_secs));
        // StdRng rather than ThreadRng: this is held across an await inside a spawned
        // task, and ThreadRng is not Send. Seeded from the clock and the filter id,
        // which is enough — jitter only has to differ between filters and between
        // processes, not resist prediction.
        let mut rng = StdRng::seed_from_u64(jitter_seed(self.id));
        // Loop state, like the backoff: the rung this filter settled on is carried across
        // refreshes so the ladder is walked once per session.
        let mut degradation = Degradation::none();

        // The stagger applies once, at startup.
        if !self.stagger.is_zero() {
            tokio::select! {
                () = self.cancel.cancelled() => return,
                () = tokio::time::sleep(self.stagger) => {}
            }
        }

        let mut wake = Wake::Startup;

        loop {
            if self.cancel.is_cancelled() {
                return;
            }

            let skip = match wake {
                Wake::Startup => self.auth_paused(),
                Wake::Timer => self.auth_paused() || self.unfocused(),
                // The user saying they fixed the token outranks a credential we last
                // saw refused.
                Wake::Manual => false,
            };

            // Set inside the `Outcome::Paused` arm below and read after, rather than
            // re-reading `auth_paused()`: that flag is written by the event loop, which
            // has not necessarily dequeued the `RefreshesPaused` this cycle just sent, so
            // reading it here could still see `false` and schedule a deadline nothing will
            // honour.
            let mut paused_now = false;

            let delay = if skip {
                tracing::trace!(filter = %self.filter.name, "refreshes paused, skipping a refresh");
                next_delay(&self.refresh, &mut rng)
            } else {
                // Cancellation drops `fetch_once`'s future — including whatever page
                // request it is mid-`await` on — rather than waiting for it to finish.
                // Without this, quitting during a refresh blocks on the network for up to
                // `MAX_PAGES` round trips, since `Tasks::shutdown` awaits this task before
                // the terminal guard is allowed to restore.
                let outcome = tokio::select! {
                    () = self.cancel.cancelled() => return,
                    outcome = self.fetch_once(&mut degradation) => outcome,
                };
                match outcome {
                    Outcome::Healthy => {
                        backoff.reset();
                        next_delay(&self.refresh, &mut rng)
                    }
                    // A filter that keeps failing backs off rather than retrying on the
                    // normal cadence, which would hammer an instance that is already
                    // unwell. `retry_after` is the server's own instruction and overrides
                    // the ladder.
                    Outcome::Failed { retry_after } => backoff.record_failure(retry_after),
                    // The next timer wake is skipped by `auth_paused()` above, so there
                    // is nothing to back off from — this delay is never acted on.
                    Outcome::Paused => {
                        paused_now = true;
                        let _ = self
                            .events
                            .send(AppEvent::RefreshesPaused { filter: self.id });
                        next_delay(&self.refresh, &mut rng)
                    }
                }
            };

            // A deadline nothing will honour is worse than none: while paused, the
            // status bar says why instead of counting down to a refresh that will not
            // happen.
            if !paused_now && !self.auth_paused() {
                let _ = self.events.send(AppEvent::RefreshScheduled {
                    filter: self.id,
                    due: std::time::Instant::now() + delay,
                });
            }

            wake = tokio::select! {
                () = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => Wake::Timer,
                // A manual request cuts the wait short. The timer restarts from here, so
                // ctrl-r also resets the cadence.
                request = manual.recv() => {
                    if request.is_none() {
                        return;
                    }
                    Wake::Manual
                }
            };
        }
    }

    /// Whether `pause_when_unfocused` currently suppresses timer-driven refreshes.
    fn unfocused(&self) -> bool {
        self.refresh.pause_when_unfocused && !self.flags.focused.load(Ordering::Relaxed)
    }

    /// Whether a runtime 401/403 has stopped every filter.
    fn auth_paused(&self) -> bool {
        self.flags.auth_paused.load(Ordering::Relaxed)
    }

    /// Run one fetch, reporting the outcome.
    async fn fetch_once(&self, degradation: &mut Degradation) -> Outcome {
        let Ok(_permit) = self.permits.acquire().await else {
            // The semaphore is closed, which only happens at shutdown.
            return Outcome::Healthy;
        };

        if self
            .events
            .send(AppEvent::FetchStarted { filter: self.id })
            .is_err()
        {
            return Outcome::Healthy;
        }

        let now = jiff::Timestamp::now();
        let result = fetch::fetch(
            &self.client,
            &self.filter,
            degradation,
            &self.current_user,
            &self.instance_url,
            now,
        )
        .await;

        match result {
            Ok(snapshot) => {
                // Written here rather than from the event loop: the loop is the render
                // loop, and a cache directory on a network home would stutter a frame
                //. A failed write is logged and forgotten — the next
                // refresh tries again, and a cold start still works.
                if let Some(dir) = &self.cache_dir {
                    let entry = crate::app::cache::Entry::of(&snapshot, now);
                    if let Err(error) = crate::app::cache::write(dir, &self.filter, &entry).await {
                        tracing::warn!(filter = %self.filter.name, %error, "could not cache snapshot");
                    }
                }

                let _ = self.events.send(AppEvent::Snapshot {
                    filter: self.id,
                    snapshot: Box::new(snapshot),
                });
                Outcome::Healthy
            }
            Err(error) => {
                // Computed before the error is moved into the event, and taken from
                // `recovery` rather than by matching variants here: that method is the
                // single statement of what each failure class means, so adding a class is
                // a compile error at this match instead of falling through to a plain
                // retry.
                let outcome = Outcome::from(error.recovery(Phase::Runtime));
                tracing::warn!(filter = %self.filter.name, %error, ?outcome, "refresh failed");
                let _ = self.events.send(AppEvent::FetchFailed {
                    filter: self.id,
                    error: Box::new(error),
                });
                outcome
            }
        }
    }
}

/// What one fetch implies for the next one's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Keep the normal jittered cadence. A success, or a partial response that still
    /// put rows on screen.
    Healthy,
    /// Wait out a failure. `retry_after` is the server's instruction when it sent one,
    /// otherwise the caller applies its own ladder.
    Failed { retry_after: Option<Duration> },
    /// The credential was refused. Every filter stops until the user asks again.
    Paused,
}

impl From<Recovery> for Outcome {
    fn from(recovery: Recovery) -> Self {
        // Only the failure classes a *fetch* can produce are reachable here; the arms are
        // spelled out anyway so that a change to the recovery table lands as a compile error.
        match recovery {
            // A partial response rendered what arrived and marked the tab, so the filter
            // is not failing and must not lose its cadence.
            Recovery::RenderPartial => Self::Healthy,

            // The whole point of this bug: the server knows when its rate-limit window
            // closes and we do not.
            Recovery::Backoff { retry_after } => Self::Failed { retry_after },

            // `fetch` walks both degradation ladders itself. Reaching the
            // worker means it ran out of rungs, so there is nothing left to try now.
            Recovery::ReduceFragment | Recovery::DegradeComplexity => {
                Self::Failed { retry_after: None }
            }

            Recovery::PauseRefreshes => Self::Paused,

            // Config failures are resolved before the runtime exists, so a fetch cannot
            // raise one. Backing off is the safe reading if that ever stops being true.
            Recovery::Fatal => Self::Failed { retry_after: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use crate::config::schema::{Filter, Scope};
    use crate::error::Error;

    fn refresh(interval: u64, jitter: u64) -> Refresh {
        Refresh {
            interval_secs: interval,
            jitter_secs: jitter,
            ..Refresh::default()
        }
    }

    /// Without jitter, N clients refreshing on the same cadence hit the instance in the
    /// same millisecond every cycle.
    #[test]
    fn jitter_spreads_refreshes_across_a_window() {
        let mut rng = StdRng::seed_from_u64(42);
        let config = refresh(300, 15);

        let delays: Vec<Duration> = (0..50).map(|_| next_delay(&config, &mut rng)).collect();

        for delay in &delays {
            assert!(
                *delay >= Duration::from_secs(300) && *delay <= Duration::from_secs(315),
                "{delay:?} outside the jitter window"
            );
        }
        let distinct: std::collections::BTreeSet<u64> =
            delays.iter().map(|d| d.as_secs()).collect();
        assert!(distinct.len() > 5, "jitter produced only {:?}", distinct);
    }

    #[test]
    fn zero_jitter_gives_the_exact_interval() {
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(
            next_delay(&refresh(300, 0), &mut rng),
            Duration::from_secs(300)
        );
    }

    /// Filters must not all fire together at startup.
    #[test]
    fn startup_is_staggered_across_filters() {
        let interval = Duration::from_secs(300);

        let delays: Vec<Duration> = (0..4).map(|i| startup_stagger(i, 4, interval)).collect();

        assert_eq!(delays[0], Duration::ZERO, "the first tab fetches at once");
        for pair in delays.windows(2) {
            assert!(pair[1] > pair[0], "not staggered: {delays:?}");
        }
    }

    /// A user watching an empty tab does not care that the load is well distributed.
    #[test]
    fn the_stagger_is_capped_so_later_filters_still_appear_promptly() {
        let interval = Duration::from_secs(3600);

        let last = startup_stagger(8, 9, interval);
        assert!(
            last <= Duration::from_secs(20),
            "the ninth filter waits {last:?} before its first fetch"
        );
    }

    #[test]
    fn a_single_filter_is_not_staggered() {
        assert_eq!(
            startup_stagger(0, 1, Duration::from_secs(300)),
            Duration::ZERO
        );
    }

    /// Holding ctrl-r must not queue fifty refreshes.
    #[tokio::test]
    async fn repeated_refresh_requests_coalesce() {
        let (tx, mut rx) = mpsc::channel(1);
        let handle = RefreshHandle { senders: vec![tx] };

        for _ in 0..50 {
            handle.request(RefreshRequest::One(0));
        }

        assert!(rx.try_recv().is_ok(), "one request arrives");
        assert!(
            rx.try_recv().is_err(),
            "the other 49 were coalesced, not queued"
        );
    }

    #[tokio::test]
    async fn refreshing_all_reaches_every_filter() {
        let mut receivers = Vec::new();
        let mut senders = Vec::new();
        for _ in 0..3 {
            let (tx, rx) = mpsc::channel(1);
            senders.push(tx);
            receivers.push(rx);
        }
        let handle = RefreshHandle { senders };

        handle.request(RefreshRequest::All);

        for (index, rx) in receivers.iter_mut().enumerate() {
            assert!(rx.try_recv().is_ok(), "filter {index} was not asked");
        }
    }

    #[tokio::test]
    async fn requesting_an_out_of_range_filter_is_ignored() {
        let (tx, _rx) = mpsc::channel(1);
        let handle = RefreshHandle { senders: vec![tx] };

        // Must not panic.
        handle.request(RefreshRequest::One(99));
        assert_eq!(handle.len(), 1);
    }

    /// N tabs must not open N simultaneous connections to a shared instance.
    #[tokio::test]
    async fn the_semaphore_bounds_concurrent_fetches() {
        let permits = Arc::new(Semaphore::new(2));

        let a = permits.clone().acquire_owned().await.unwrap();
        let b = permits.clone().acquire_owned().await.unwrap();
        assert_eq!(permits.available_permits(), 0);

        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "a third fetch must wait"
        );

        drop(a);
        assert!(permits.clone().try_acquire_owned().is_ok());
        drop(b);
    }

    /// The bug: the worker reduced the fetch error to a bool before choosing a delay, so
    /// a 429's `Retry-After` was dropped and the filter retried at 2s, 4s, 8s… inside a
    /// window the server had explicitly closed. That is how a client gets banned.
    #[test]
    fn a_rate_limit_schedules_the_servers_window_not_the_ladder() {
        let server = Duration::from_secs(45);
        let outcome = Outcome::from(
            Error::RateLimited {
                retry_after: Some(server),
            }
            .recovery(Phase::Runtime),
        );
        assert_eq!(
            outcome,
            Outcome::Failed {
                retry_after: Some(server)
            }
        );

        // And it reaches the delay, rather than being carried and then ignored.
        let mut backoff = Backoff::new(Duration::from_secs(300));
        let Outcome::Failed { retry_after } = outcome else {
            panic!("a rate limit is a failure");
        };
        assert_eq!(backoff.record_failure(retry_after), server);
        assert_ne!(
            backoff.record_failure(retry_after),
            Duration::from_secs(4),
            "the ladder must not override the server"
        );
    }

    /// A 429 with no header still has to back off — there is just nothing better to use
    /// than our own ladder.
    #[test]
    fn a_rate_limit_without_a_header_falls_back_to_the_ladder() {
        let outcome =
            Outcome::from(Error::RateLimited { retry_after: None }.recovery(Phase::Runtime));
        assert_eq!(outcome, Outcome::Failed { retry_after: None });

        let mut backoff = Backoff::new(Duration::from_secs(300));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(2));
    }

    /// One case per row that a fetch can actually produce. Pinned as a table because
    /// the mapping is the whole fix: reducing any of these to "failed" is what dropped
    /// the server's instruction in the first place.
    #[test]
    fn every_fetch_failure_maps_to_its_spec_behaviour() {
        use crate::config::token::TokenSource;
        use crate::error::{GraphQlError, LimitKind};

        let cases = [
            (
                Error::RateLimited {
                    retry_after: Some(Duration::from_secs(30)),
                },
                Outcome::Failed {
                    retry_after: Some(Duration::from_secs(30)),
                },
            ),
            (
                Error::Http { status: 503 },
                Outcome::Failed { retry_after: None },
            ),
            (
                Error::Network("connection refused".into()),
                Outcome::Failed { retry_after: None },
            ),
            (
                Error::GraphQl {
                    errors: vec![GraphQlError::new("boom")],
                },
                Outcome::Failed { retry_after: None },
            ),
            (
                Error::UnknownField {
                    fields: vec!["approvalsLeft".into()],
                },
                Outcome::Failed { retry_after: None },
            ),
            (
                Error::LimitExceeded {
                    kind: LimitKind::Complexity,
                    limit: Some(250),
                },
                Outcome::Failed { retry_after: None },
            ),
            // The one failure that is not a failure: rows reached the screen.
            (
                Error::GraphQlPartial {
                    errors: vec![GraphQlError::new("boom")],
                },
                Outcome::Healthy,
            ),
            // The credential was refused: every filter stops, not just this one.
            (
                Error::Unauthorized {
                    status: 401,
                    instance: "https://gitlab.example.com".into(),
                    token_source: TokenSource::GitlabTokenEnv,
                },
                Outcome::Paused,
            ),
        ];

        for (error, want) in cases {
            let rendered = error.to_string();
            assert_eq!(
                Outcome::from(error.recovery(Phase::Runtime)),
                want,
                "wrong outcome for: {rendered}"
            );
        }
    }

    /// A partial response must not cost the filter its cadence: it rendered rows, and
    /// treating it as a failure would drop a healthy tab onto the backoff ladder.
    #[test]
    fn a_partial_response_keeps_the_normal_cadence() {
        let mut backoff = Backoff::new(Duration::from_secs(300));
        backoff.record_failure(None);

        // What the worker does on Outcome::Healthy.
        backoff.reset();
        assert!(!backoff.is_failing());
    }

    /// A filter that keeps failing must not retry on the normal cadence, which would
    /// hammer an instance that is already unwell.
    #[test]
    fn failures_back_off_and_success_restores_the_cadence() {
        let mut backoff = Backoff::new(Duration::from_secs(300));

        assert_eq!(backoff.record_failure(None), Duration::from_secs(2));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(4));
        assert!(backoff.is_failing());

        backoff.reset();
        assert!(!backoff.is_failing());
    }

    #[test]
    fn the_backoff_cap_is_the_refresh_interval() {
        let interval = Duration::from_secs(60);
        let mut backoff = Backoff::new(interval);

        for _ in 0..20 {
            assert!(backoff.record_failure(None) <= interval);
        }
    }

    /// Notifications diff against a previous snapshot, so a tab that never fetches
    /// never has one and can never notify.
    #[test]
    fn every_configured_filter_gets_a_worker() {
        let filters = vec![
            Filter::named("One", Scope::Assigned),
            Filter::named("Two", Scope::ReviewRequested),
            Filter::named("Three", Scope::Authored),
        ];
        let config = Config {
            filters,
            ..Config::default()
        };

        // spawn() needs a runtime; assert the shape it will produce instead.
        assert_eq!(config.filters.len(), 3);
        let stagger: Vec<Duration> = (0..3)
            .map(|i| startup_stagger(i, 3, Duration::from_secs(config.refresh.interval_secs)))
            .collect();
        assert_eq!(stagger.len(), 3);
        assert!(stagger.iter().all(|d| *d <= Duration::from_secs(6)));
    }

    #[tokio::test]
    async fn spawning_creates_one_sender_per_filter() {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let gitlab = Gitlab {
            url: "http://127.0.0.1:1".into(),
            token: Some("glpat-test".into()),
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let config = Config {
            filters: vec![
                Filter::named("One", Scope::Assigned),
                Filter::named("Two", Scope::Authored),
            ],
            gitlab,
            ..Config::default()
        };

        let (events, _rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let handle = spawn(
            &mut tasks,
            events,
            client,
            &config,
            "me".to_owned(),
            None,
            Flags {
                focused: focused(),
                auth_paused: unpaused(),
            },
        );

        assert_eq!(handle.len(), 2);
        tasks.shutdown().await;
    }

    /// The bug end to end, at the level it actually broke.
    ///
    /// The mapping tests above check `Outcome` and `Backoff` in isolation, and both were
    /// already correct — `Backoff::record_failure` honoured `retry_after` and had a
    /// passing test. What was broken was the wiring between them, so the only test that
    /// can fail for the original reason is one that drives the real worker and reads the
    /// delay it actually schedules.
    #[tokio::test]
    async fn a_worker_schedules_the_servers_retry_after_from_a_real_429() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const RETRY_AFTER: u64 = 45;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("retry-after", RETRY_AFTER.to_string()),
            )
            .mount(&server)
            .await;

        let (events, mut rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let _handle = spawn_against(&server.uri(), &events, &mut tasks);

        let due = tokio::time::timeout(Duration::from_secs(10), next_scheduled(&mut rx))
            .await
            .expect("the worker should schedule a retry promptly");
        let delay = due.saturating_duration_since(std::time::Instant::now());

        // The ladder's first rung is 2s, so anything near it means the header was dropped.
        assert!(
            delay > Duration::from_secs(30),
            "scheduled a retry in {delay:?}; the server asked for {RETRY_AFTER}s, so the \
             Retry-After header is being ignored and the ladder used instead"
        );
        assert!(
            delay <= Duration::from_secs(RETRY_AFTER),
            "scheduled {delay:?}, longer than the server asked for"
        );

        tasks.shutdown().await;
    }

    /// A transport failure with no server instruction still walks our own ladder, which
    /// is what stops the assertion above from passing for the wrong reason.
    #[tokio::test]
    async fn a_worker_falls_back_to_the_ladder_when_the_server_says_nothing() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (events, mut rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let _handle = spawn_against(&server.uri(), &events, &mut tasks);

        let due = tokio::time::timeout(Duration::from_secs(10), next_scheduled(&mut rx))
            .await
            .expect("the worker should schedule a retry promptly");
        let delay = due.saturating_duration_since(std::time::Instant::now());

        assert!(
            delay <= Duration::from_secs(3),
            "a 500 with no Retry-After should use the 2s first rung, got {delay:?}"
        );

        tasks.shutdown().await;
    }

    /// The bug end to end: a real 401 reports the pause and, once the loop acts on it —
    /// flipping the shared flag, as `App::set_paused` does — the worker actually stops
    /// rather than falling back to the ladder with a credential already refused.
    #[tokio::test]
    async fn a_worker_stops_requesting_once_the_loop_acts_on_a_real_401() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        struct Unauthorized(Arc<AtomicUsize>);
        impl Respond for Unauthorized {
            fn respond(&self, _: &Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::Relaxed);
                ResponseTemplate::new(401)
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(Unauthorized(Arc::clone(&hits)))
            .mount(&server)
            .await;

        let (events, mut rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let auth_paused = Arc::new(AtomicBool::new(false));
        let _handle =
            spawn_auth_paused(&server.uri(), &events, &mut tasks, Arc::clone(&auth_paused));

        let filter = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await.expect("the channel should not close") {
                    AppEvent::RefreshesPaused { filter } => return filter,
                    AppEvent::FetchStarted { .. }
                    | AppEvent::FetchFailed { .. }
                    | AppEvent::RefreshScheduled { .. } => {}
                    other => panic!("unexpected event before the pause: {other:?}"),
                }
            }
        })
        .await
        .expect("the worker should report the pause promptly");
        assert_eq!(filter, 0);

        // What the event loop does on receiving `RefreshesPaused`.
        auth_paused.store(true, Ordering::Relaxed);
        // Let a fetch already in flight land before taking the baseline.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = hits.load(Ordering::Relaxed);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            hits.load(Ordering::Relaxed),
            before,
            "a paused worker kept sending a credential the instance already refused"
        );

        tasks.shutdown().await;
    }

    /// Unlike `pause_when_unfocused`, which lets the first fetch through so the table
    /// has something to show, an auth pause already known at startup must suppress even
    /// that fetch — there is nothing to gain from a request already known to be refused.
    #[tokio::test]
    async fn an_auth_paused_worker_skips_even_its_startup_fetch() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let auth_paused = Arc::new(AtomicBool::new(true));
        let _handle = spawn_auth_paused(&server.uri(), &events, &mut tasks, auth_paused);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            hits.load(Ordering::Relaxed),
            0,
            "an auth-paused worker must not fetch at all"
        );

        tasks.shutdown().await;
    }

    /// The user saying they fixed the token outranks a credential we last saw refused.
    #[tokio::test]
    async fn a_manual_request_breaks_an_auth_pause() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let auth_paused = Arc::new(AtomicBool::new(true));
        let handle = spawn_auth_paused(&server.uri(), &events, &mut tasks, auth_paused);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        handle.request(RefreshRequest::One(0));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            hits.load(Ordering::Relaxed) > 0,
            "a manual refresh was swallowed by the auth pause"
        );

        tasks.shutdown().await;
    }

    fn focused() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }

    fn unpaused() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// One worker pointed at a mock instance, with a literal token.
    fn spawn_against(uri: &str, events: &EventSender, tasks: &mut Tasks) -> RefreshHandle {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let gitlab = Gitlab {
            url: uri.to_owned(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let config = Config {
            filters: vec![Filter::named("One", Scope::Assigned)],
            gitlab,
            ..Config::default()
        };

        spawn(
            tasks,
            events.clone(),
            client,
            &config,
            "me".to_owned(),
            None,
            Flags {
                focused: focused(),
                auth_paused: unpaused(),
            },
        )
    }

    /// `pause_when_unfocused` skips timer ticks while the terminal is
    /// unfocused. The startup fetch still runs — it is what fills the table — so the
    /// assertion is that the request count stops at one across many intervals.
    #[tokio::test]
    async fn an_unfocused_worker_skips_its_timer_ticks_when_configured_to_pause() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let focus = Arc::new(AtomicBool::new(false));
        let _handle = spawn_paused(&server.uri(), &events, &mut tasks, Arc::clone(&focus), true);

        // Several refresh intervals' worth of wall clock.
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "only the startup fetch should have run while unfocused"
        );
        tasks.shutdown().await;
    }

    /// The control: the same worker, focused, keeps refreshing. Without this the test
    /// above would pass just as well if the worker had died.
    #[tokio::test]
    async fn a_focused_worker_keeps_refreshing_on_the_timer() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let _handle = spawn_paused(&server.uri(), &events, &mut tasks, focused(), true);

        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            hits.load(Ordering::Relaxed) > 1,
            "a focused worker refreshed only {} time(s)",
            hits.load(Ordering::Relaxed)
        );
        tasks.shutdown().await;
    }

    /// `pause_when_unfocused` defaults to false, so an unfocused terminal keeps polling
    /// unless the user asked for the pause.
    #[tokio::test]
    async fn an_unfocused_worker_keeps_refreshing_when_the_pause_is_off() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let focus = Arc::new(AtomicBool::new(false));
        let _handle = spawn_paused(&server.uri(), &events, &mut tasks, focus, false);

        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(hits.load(Ordering::Relaxed) > 1);
        tasks.shutdown().await;
    }

    /// A manual request is the user waiting on an answer, so the pause must not swallow
    /// it — this is the path a focus gain takes too.
    #[tokio::test]
    async fn a_manual_request_is_served_even_while_paused_and_unfocused() {
        let (server, hits) = counting_server().await;

        let mut tasks = Tasks::new();
        let (events, _rx) = crate::app::event::channel();
        let focus = Arc::new(AtomicBool::new(false));
        let handle = spawn_paused(&server.uri(), &events, &mut tasks, focus, true);

        // Let the startup fetch land and the worker settle into its paused wait.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let before = hits.load(Ordering::Relaxed);

        handle.request(RefreshRequest::One(0));
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            hits.load(Ordering::Relaxed) > before,
            "a manual refresh was swallowed by the pause"
        );
        tasks.shutdown().await;
    }

    /// A server that answers every query and counts the requests.
    async fn counting_server() -> (wiremock::MockServer, Arc<AtomicUsize>) {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));

        struct Count(Arc<AtomicUsize>);
        impl Respond for Count {
            fn respond(&self, _: &Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::Relaxed);
                // A well-formed empty result, so the fetch succeeds and the worker takes
                // the normal-cadence path rather than the backoff ladder.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": { "currentUser": { "assignedMergeRequests": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": []
                    }}}
                }))
            }
        }

        Mock::given(method("POST"))
            .respond_with(Count(Arc::clone(&hits)))
            .mount(&server)
            .await;

        (server, hits)
    }

    /// One worker with a short interval and an explicit focus flag and pause policy.
    fn spawn_paused(
        uri: &str,
        events: &EventSender,
        tasks: &mut Tasks,
        focus: Arc<AtomicBool>,
        pause_when_unfocused: bool,
    ) -> RefreshHandle {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let gitlab = Gitlab {
            url: uri.to_owned(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let config = Config {
            filters: vec![Filter::named("One", Scope::Assigned)],
            gitlab,
            // Sub-second so the test does not wait out a real interval. The floor
            // applies to configured values, not to one built in a test.
            refresh: Refresh {
                interval_secs: 0,
                jitter_secs: 0,
                pause_when_unfocused,
                ..Refresh::default()
            },
            ..Config::default()
        };

        spawn(
            tasks,
            events.clone(),
            client,
            &config,
            "me".to_owned(),
            None,
            Flags {
                focused: focus,
                auth_paused: unpaused(),
            },
        )
    }

    /// One worker with a sub-second interval and an explicit auth-pause flag, focused
    /// throughout — isolates the auth pause from `pause_when_unfocused`.
    fn spawn_auth_paused(
        uri: &str,
        events: &EventSender,
        tasks: &mut Tasks,
        auth_paused: Arc<AtomicBool>,
    ) -> RefreshHandle {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let gitlab = Gitlab {
            url: uri.to_owned(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let config = Config {
            filters: vec![Filter::named("One", Scope::Assigned)],
            gitlab,
            refresh: Refresh {
                interval_secs: 0,
                jitter_secs: 0,
                ..Refresh::default()
            },
            ..Config::default()
        };

        spawn(
            tasks,
            events.clone(),
            client,
            &config,
            "me".to_owned(),
            None,
            Flags {
                focused: focused(),
                auth_paused,
            },
        )
    }

    // ------------------------------------------------------- refresh_on_focus policy

    /// Refresh on regaining focus, *if stale*. Refetching everything on
    /// every alt-tab would turn a window manager into a load generator.
    #[test]
    fn only_stale_filters_are_refreshed_on_a_focus_gain() {
        use crate::app::state::Tabs;
        use crate::config::schema::Sort;

        let interval = 300;
        let policy = Refresh {
            interval_secs: interval,
            refresh_on_focus: true,
            ..Refresh::default()
        };
        let filters = vec![
            Filter::named("Fresh", Scope::Assigned),
            Filter::named("Stale", Scope::Authored),
            Filter::named("Never", Scope::ReviewRequested),
        ];
        let mut tabs = Tabs::new(&filters, Sort::default(), false, None);

        let now = std::time::Instant::now();
        tabs.get_mut(0).unwrap().apply_rows(Vec::new(), now);
        tabs.get_mut(1)
            .unwrap()
            .apply_rows(Vec::new(), now - Duration::from_secs(interval + 1));
        // Tab 2 has never fetched, so it is stale whatever is on screen.

        assert_eq!(
            filters_to_refresh_on_focus(&tabs, &policy, now, false),
            [1, 2],
            "the fresh tab must not be refetched"
        );
    }

    /// A focus gain must not re-send a credential the instance has already refused —
    /// that is the alt-tab-as-ban-generator case the auth-pause behavior exists to avoid.
    #[test]
    fn a_focus_gain_requests_nothing_while_auth_paused() {
        use crate::app::state::Tabs;
        use crate::config::schema::Sort;

        let policy = Refresh {
            refresh_on_focus: true,
            ..Refresh::default()
        };
        let tabs = Tabs::new(
            &[Filter::named("Never", Scope::Assigned)],
            Sort::default(),
            false,
            None,
        );

        assert!(
            filters_to_refresh_on_focus(&tabs, &policy, std::time::Instant::now(), true).is_empty()
        );
    }

    #[test]
    fn refresh_on_focus_off_requests_nothing() {
        use crate::app::state::Tabs;
        use crate::config::schema::Sort;

        let policy = Refresh {
            refresh_on_focus: false,
            ..Refresh::default()
        };
        let tabs = Tabs::new(
            &[Filter::named("Never", Scope::Assigned)],
            Sort::default(),
            false,
            None,
        );

        assert!(
            filters_to_refresh_on_focus(&tabs, &policy, std::time::Instant::now(), false)
                .is_empty(),
            "a tab that has never fetched is stale, but the setting is off"
        );
    }

    /// A filter already fetching has its result seconds away; asking again would queue a
    /// second fetch behind the one in flight.
    #[test]
    fn a_filter_already_fetching_is_not_asked_again() {
        use crate::app::state::Tabs;
        use crate::config::schema::Sort;

        let policy = Refresh {
            refresh_on_focus: true,
            ..Refresh::default()
        };
        let mut tabs = Tabs::new(
            &[Filter::named("One", Scope::Assigned)],
            Sort::default(),
            false,
            None,
        );
        tabs.get_mut(0).unwrap().begin_fetch();

        assert!(
            filters_to_refresh_on_focus(&tabs, &policy, std::time::Instant::now(), false)
                .is_empty()
        );
    }

    /// The `due` of the first `RefreshScheduled`, ignoring the fetch lifecycle events
    /// that precede it.
    async fn next_scheduled(rx: &mut crate::app::event::EventReceiver) -> Instant {
        while let Some(event) = rx.recv().await {
            match event {
                AppEvent::RefreshScheduled { due, .. } => return due,
                AppEvent::FetchStarted { .. }
                | AppEvent::FetchFailed { .. }
                | AppEvent::RefreshesPaused { .. } => {}
                other => panic!("unexpected event before a schedule: {other:?}"),
            }
        }
        panic!("the worker never scheduled a refresh");
    }

    /// Shutdown must stop the workers, including one sleeping out its stagger.
    #[tokio::test]
    async fn shutdown_stops_the_workers() {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let gitlab = Gitlab {
            url: "http://127.0.0.1:1".into(),
            token: Some("glpat-test".into()),
            timeout_secs: 1,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let config = Config {
            filters: vec![Filter::named("One", Scope::Assigned)],
            gitlab,
            ..Config::default()
        };

        let (events, _rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let _handle = spawn(
            &mut tasks,
            events,
            client,
            &config,
            "me".to_owned(),
            None,
            Flags {
                focused: focused(),
                auth_paused: unpaused(),
            },
        );

        tokio::time::timeout(Duration::from_secs(10), tasks.shutdown())
            .await
            .expect("workers should stop promptly");
    }
}
