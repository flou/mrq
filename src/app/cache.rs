//! Snapshot persistence and the warm first paint.
//!
//! The target is a first painted frame in under 50 ms, which is unreachable if the
//! first paint waits on a network round trip. After each successful
//! fetch a filter's rows go to `$XDG_CACHE_HOME/mrq/<slug>.json`; at startup they come
//! back, are painted immediately and are marked as cached until a live fetch lands.
//!
//! # A cache is disposable, so nothing here is fatal
//!
//! Every failure path ends in "treat it as absent". A missing file is the normal first
//! run. A corrupt one — a half-written file from a crash, a format from an older `mrq`,
//! something a user edited — is deleted and forgotten. Refusing to start because a
//! *cache* would not parse would be the worst possible reading of an optimisation.
//!
//! # No token, by construction
//!
//! [`Entry`] holds merge requests and a timestamp. The token cannot reach it even by
//! mistake: `config::token::Token` has no `Serialize` impl, so a future field that tried
//! to carry one would not compile.

use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::app::state::Tabs;
use crate::config::schema::Filter;
use crate::gitlab::fetch::Snapshot;
use crate::gitlab::model::MergeRequest;
use crate::gitlab::query::Fragment;

/// The on-disk format version.
///
/// Bumping it makes every existing file fail the check below and be discarded, which is
/// exactly what should happen when the shape changes: a cache is an optimisation, and
/// migrating one costs more than refetching it.
const VERSION: u32 = 1;

/// Entries older than this are ignored and deleted.
const MAX_AGE: SignedDuration = SignedDuration::from_hours(24 * 7);

/// One filter's cached snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub version: u32,
    /// When the fetch that produced these rows completed, as wall-clock time. An
    /// `Instant` would be meaningless here — it is not comparable across processes.
    pub fetched_at: Timestamp,
    /// Whether `max_results` cut the list short, so a warm render can say the list is
    /// capped rather than implying it is complete.
    pub truncated: bool,
    /// The query shape these rows came from, so a degraded warm render stays marked.
    pub fragment: Fragment,
    pub merge_requests: Vec<MergeRequest>,
}

impl Entry {
    /// Capture a successful fetch.
    pub fn of(snapshot: &Snapshot, fetched_at: Timestamp) -> Self {
        Self {
            version: VERSION,
            fetched_at,
            truncated: snapshot.truncated,
            fragment: snapshot.fragment.clone(),
            merge_requests: snapshot.merge_requests.clone(),
        }
    }

    /// How old these rows are.
    fn age(&self, now: Timestamp) -> SignedDuration {
        now.duration_since(self.fetched_at)
    }
}

/// The cache file for one filter.
///
/// `Filter::slug` is filesystem-safe and hash-disambiguated, so two filters whose names
/// differ only in punctuation get different files.
pub fn path_of(dir: &Path, filter: &Filter) -> PathBuf {
    dir.join(format!("{}.json", filter.slug()))
}

/// Persist a successful fetch.
///
/// Written to a temporary file and renamed, so a reader — this process on its next run,
/// or a second `mrq` — never sees a half-written file. Not fsynced: losing the last
/// refresh to a power cut costs one round trip on the next start.
pub async fn write(dir: &Path, filter: &Filter, entry: &Entry) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let json = serde_json::to_vec(entry).map_err(std::io::Error::other)?;

    let path = path_of(dir, filter);
    // The pid makes this unique per process: two `mrq` instances refreshing the same
    // filter must not share a temp file. The `rename` below is atomic, but the content
    // underneath a shared name would not be — one instance's `truncate(true)` open can
    // land between another's write and its rename, corrupting both.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));

    // Owner-only: the enclosing directory is already 0700, and this is defence in depth
    // for the merge-request titles and branch names inside.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .await?;
    file.write_all(&json).await?;
    drop(file);

    let result = tokio::fs::rename(&tmp, &path).await;
    if result.is_err() {
        // Best-effort: the write already failed, and there is nowhere better to report a
        // second failure here. Leaving the temp file around costs disk space forever
        // rather than one round trip.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Read a filter's cached snapshot, or `None` if there is nothing usable.
///
/// Deletes the file when it is unreadable, from another format version, or expired, so a
/// bad entry costs one start rather than every start.
pub fn read(dir: &Path, filter: &Filter, now: Timestamp) -> Option<Entry> {
    let path = path_of(dir, filter);

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // The normal first run. Anything else is worth a line, since a cache that is
        // silently never read looks exactly like one that is working.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "cache unreadable");
            return None;
        }
    };

    let entry: Entry = match serde_json::from_slice(&bytes) {
        Ok(entry) => entry,
        Err(error) => {
            discard(&path, &format!("not parseable: {error}"));
            return None;
        }
    };

    if entry.version != VERSION {
        discard(&path, &format!("format version {}", entry.version));
        return None;
    }

    let age = entry.age(now);
    if age > MAX_AGE {
        discard(&path, &format!("{} days old", age.as_hours() / 24));
        return None;
    }
    // A timestamp in the future means a clock that moved, not a fresh entry. Kept rather
    // than discarded — the rows are still the last thing the instance said — but the age
    // is clamped at zero by the caller so the status bar cannot show a negative one.
    if age.is_negative() {
        tracing::debug!(path = %path.display(), "cache timestamp is in the future");
    }

    Some(entry)
}

/// Delete an unusable entry, logging why.
fn discard(path: &Path, reason: &str) {
    tracing::info!(path = %path.display(), reason, "discarding cache entry");
    if let Err(error) = std::fs::remove_file(path) {
        tracing::warn!(path = %path.display(), %error, "could not delete cache entry");
    }
}

/// Load every filter's cache into its tab, for the first paint.
///
/// `current_user` re-derives the per-user flags: an entry may have been written by a
/// different account, and stale flags would make the ASSIGNED column lie. It is `None`
/// before the identity probe has landed, which is the normal case: the first frame must
/// not wait on a round trip. The flags in the file were derived by the run that wrote
/// it and are this account's unless the token changed between runs; `AppEvent::Identified`
/// re-derives them once the probe lands, so a mismatch lasts one round trip rather than
/// the session.
///
/// A rejected alternative worth recording: blanking the ASSIGNED column until the probe
/// lands. It makes the common case — same account, flags already correct — flicker to
/// fix a case that essentially never happens.
///
/// Returns how many tabs were populated, for the startup log line.
pub fn warm(
    tabs: &mut Tabs,
    filters: &[Filter],
    dir: &Path,
    current_user: Option<&str>,
    now: Timestamp,
) -> usize {
    let mut warmed = 0;

    for (index, filter) in filters.iter().enumerate() {
        let Some(entry) = read(dir, filter, now) else {
            continue;
        };
        let Some(tab) = tabs.get_mut(index) else {
            continue;
        };

        // A clock that moved can date an entry in the future. Zero reads as "just now",
        // which is the least wrong thing to say about rows we cannot date.
        let age = entry.age(now).unsigned_abs();

        let mut rows = entry.merge_requests;
        if let Some(current_user) = current_user {
            for row in &mut rows {
                row.recompute_derived(current_user);
            }
        }

        tab.apply_cached(rows, age, entry.truncated, entry.fragment);
        warmed += 1;
    }

    warmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Scope, Sort};
    use crate::gitlab::model::fixtures::mr;
    use crate::gitlab::wire::Anomalies;

    const ME: &str = "me";

    fn now() -> Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn filter(name: &str) -> Filter {
        Filter::named(name, Scope::Assigned)
    }

    fn snapshot(rows: Vec<MergeRequest>) -> Snapshot {
        Snapshot {
            merge_requests: rows,
            truncated: false,
            fragment: Fragment::full(),
            partial: false,
            anomalies: Anomalies::default(),
        }
    }

    fn entry(rows: Vec<MergeRequest>, fetched_at: Timestamp) -> Entry {
        Entry::of(&snapshot(rows), fetched_at)
    }

    /// A tempdir standing in for the cache directory.
    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn tabs_for(filters: &[Filter]) -> Tabs {
        Tabs::new(filters, Sort::default(), false, None)
    }

    // ------------------------------------------------------------------- round trip

    #[tokio::test]
    async fn a_written_entry_reads_back_unchanged() {
        let tmp = dir();
        let f = filter("Assigned");
        let written = entry(vec![mr("a", "someone"), mr("b", ME)], now());

        write(tmp.path(), &f, &written).await.unwrap();
        let read_back = read(tmp.path(), &f, now()).expect("just written");

        assert_eq!(read_back, written);
    }

    /// A degraded query must stay marked across a restart, or the warm render shows
    /// blanks for a few seconds with no explanation.
    #[tokio::test]
    async fn the_query_shape_and_truncation_survive_the_round_trip() {
        let tmp = dir();
        let f = filter("Assigned");
        let mut snap = snapshot(vec![mr("a", ME)]);
        snap.truncated = true;
        snap.fragment = Fragment::full().excluding(["userNotesCount"]);

        write(tmp.path(), &f, &Entry::of(&snap, now()))
            .await
            .unwrap();
        let got = read(tmp.path(), &f, now()).unwrap();

        assert!(got.truncated);
        assert!(got.fragment.is_degraded());
        assert_eq!(got.fragment.lost(), snap.fragment.lost());
    }

    #[tokio::test]
    async fn the_file_is_owner_only_and_leaves_no_temporary_behind() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = dir();
        let f = filter("Assigned");
        write(tmp.path(), &f, &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();

        let path = path_of(tmp.path(), &f);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Two filters whose names differ only in punctuation are two tabs and
    /// must not share one file.
    #[tokio::test]
    async fn each_filter_gets_its_own_collision_free_file() {
        let tmp = dir();
        let a = filter("Team / Platform");
        let b = filter("Team: Platform");

        assert_ne!(path_of(tmp.path(), &a), path_of(tmp.path(), &b));

        write(tmp.path(), &a, &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();
        write(tmp.path(), &b, &entry(vec![mr("b", ME)], now()))
            .await
            .unwrap();

        let rows_of = |f| {
            read(tmp.path(), f, now())
                .unwrap()
                .merge_requests
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(rows_of(&a), ["a"]);
        assert_eq!(rows_of(&b), ["b"], "the second did not overwrite the first");

        for path in [path_of(tmp.path(), &a), path_of(tmp.path(), &b)] {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!name.contains('/'), "{name} would escape the cache dir");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
                "{name} is not filesystem-safe"
            );
        }
    }

    /// The token has no `Serialize` impl, so this is structural rather than
    /// a promise — but the file is what a user would paste into a bug report, so it is
    /// worth asserting on the bytes.
    #[tokio::test]
    async fn the_cache_file_contains_no_credential() {
        let tmp = dir();
        let f = filter("Assigned");
        write(tmp.path(), &f, &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();

        let raw = std::fs::read_to_string(path_of(tmp.path(), &f)).unwrap();
        for needle in ["glpat", "token", "Authorization", "Bearer"] {
            assert!(
                !raw.to_lowercase().contains(&needle.to_lowercase()),
                "`{needle}` appears in the cache file"
            );
        }
    }

    // ------------------------------------------------------------------- rejection

    #[test]
    fn a_missing_file_is_simply_absent() {
        let tmp = dir();
        assert!(read(tmp.path(), &filter("Assigned"), now()).is_none());
    }

    /// A half-written file from a crash, or something a user edited. Refusing to start
    /// over a cache would be the worst possible reading of an optimisation.
    #[test]
    fn a_corrupt_file_is_deleted_and_treated_as_absent() {
        let tmp = dir();
        let f = filter("Assigned");
        let path = path_of(tmp.path(), &f);

        for garbage in ["", "{", "null", "not json at all", r#"{"version":1}"#] {
            std::fs::write(&path, garbage).unwrap();

            assert!(
                read(tmp.path(), &f, now()).is_none(),
                "accepted garbage: {garbage:?}"
            );
            assert!(
                !path.exists(),
                "a bad entry must not survive to break the next start too: {garbage:?}"
            );
        }
    }

    /// A file truncated partway through valid JSON — the realistic crash artefact.
    #[tokio::test]
    async fn a_truncated_file_is_deleted_and_treated_as_absent() {
        let tmp = dir();
        let f = filter("Assigned");
        write(tmp.path(), &f, &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();

        let path = path_of(tmp.path(), &f);
        let whole = std::fs::read(&path).unwrap();
        std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();

        assert!(read(tmp.path(), &f, now()).is_none());
        assert!(!path.exists());
    }

    /// An older `mrq` wrote a different shape. Discarding costs one round trip;
    /// migrating a cache costs more than refetching it.
    #[test]
    fn another_format_version_is_deleted_and_treated_as_absent() {
        let tmp = dir();
        let f = filter("Assigned");
        let path = path_of(tmp.path(), &f);

        let mut stale = entry(vec![mr("a", ME)], now());
        stale.version = VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

        assert!(read(tmp.path(), &f, now()).is_none());
        assert!(!path.exists());
    }

    /// Entries older than 7 days are ignored *and* deleted.
    #[tokio::test]
    async fn an_expired_entry_is_deleted_and_treated_as_absent() {
        let tmp = dir();
        let f = filter("Assigned");
        let eight_days_ago = now() - SignedDuration::from_hours(24 * 8);

        write(tmp.path(), &f, &entry(vec![mr("a", ME)], eight_days_ago))
            .await
            .unwrap();

        assert!(read(tmp.path(), &f, now()).is_none());
        assert!(!path_of(tmp.path(), &f).exists(), "ignored but not deleted");
    }

    #[tokio::test]
    async fn an_entry_just_inside_the_window_is_kept() {
        let tmp = dir();
        let f = filter("Assigned");
        let almost = now() - SignedDuration::from_hours(24 * 7 - 1);

        write(tmp.path(), &f, &entry(vec![mr("a", ME)], almost))
            .await
            .unwrap();

        assert!(read(tmp.path(), &f, now()).is_some());
        assert!(path_of(tmp.path(), &f).exists());
    }

    /// A clock that moved backwards must not make a usable entry look like the future and
    /// get thrown away — the rows are still the last thing the instance said.
    #[tokio::test]
    async fn an_entry_from_the_future_is_kept() {
        let tmp = dir();
        let f = filter("Assigned");
        let ahead = now() + SignedDuration::from_hours(3);

        write(tmp.path(), &f, &entry(vec![mr("a", ME)], ahead))
            .await
            .unwrap();

        assert!(read(tmp.path(), &f, now()).is_some());
    }

    // ---------------------------------------------------------------- the warm start

    #[tokio::test]
    async fn warming_populates_each_tab_from_its_own_file() {
        let tmp = dir();
        let filters = vec![filter("Assigned"), filter("Reviewing")];
        write(
            tmp.path(),
            &filters[0],
            &entry(vec![mr("a", ME), mr("b", ME)], now()),
        )
        .await
        .unwrap();
        write(tmp.path(), &filters[1], &entry(vec![mr("c", ME)], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        let warmed = warm(&mut tabs, &filters, tmp.path(), Some(ME), now());

        assert_eq!(warmed, 2);
        assert_eq!(tabs.get(0).unwrap().all().len(), 2);
        assert_eq!(tabs.get(1).unwrap().all().len(), 1);
        assert_eq!(tabs.get(1).unwrap().all()[0].id, "c");
    }

    #[tokio::test]
    async fn a_filter_with_no_cache_leaves_its_tab_empty() {
        let tmp = dir();
        let filters = vec![filter("Assigned"), filter("Reviewing")];
        write(tmp.path(), &filters[1], &entry(vec![mr("c", ME)], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        assert_eq!(warm(&mut tabs, &filters, tmp.path(), Some(ME), now()), 1);

        assert!(tabs.get(0).unwrap().all().is_empty());
        assert!(tabs.get(0).unwrap().cached_age().is_none());
        assert_eq!(tabs.get(1).unwrap().all().len(), 1);
    }

    /// A cache written by another account carries that account's derived
    /// flags, and reusing them would make the ASSIGNED column lie about whose MR it is.
    #[tokio::test]
    async fn warming_recomputes_the_per_user_flags_for_this_account() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];

        // Written by "someone-else", for whom this row is authored-by-me.
        let mut row = mr("a", "someone-else");
        row.assignees = vec!["someone-else".to_owned()];
        row.recompute_derived("someone-else");
        assert!(row.authored_by_me(), "the fixture is set up wrong");

        write(tmp.path(), &filters[0], &entry(vec![row], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), Some(ME), now());

        let loaded = &tabs.get(0).unwrap().all()[0];
        assert!(
            !loaded.authored_by_me(),
            "flags must be for the running user"
        );
        assert!(!loaded.assigned_to_me());
    }

    /// Before the identity probe has landed there is no username to recompute against,
    /// and the first paint must not wait on one — so the flags in the file are trusted
    /// as they are. The inverse of the test above.
    #[tokio::test]
    async fn warming_without_an_identity_trusts_the_flags_as_cached() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];

        let mut row = mr("a", "someone-else");
        row.assignees = vec!["someone-else".to_owned()];
        row.recompute_derived("someone-else");
        assert!(row.authored_by_me(), "the fixture is set up wrong");

        write(tmp.path(), &filters[0], &entry(vec![row], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), None, now());

        let loaded = &tabs.get(0).unwrap().all()[0];
        assert!(
            loaded.authored_by_me(),
            "no identity yet, so the cached flags are left untouched"
        );
    }

    /// The rows are on screen but marked, and no live fetch has landed, so
    /// the tab must not claim to be `Loaded`.
    #[tokio::test]
    async fn a_warmed_tab_is_marked_as_cached_rather_than_loaded() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];
        let three_hours_ago = now() - SignedDuration::from_hours(3);
        write(
            tmp.path(),
            &filters[0],
            &entry(vec![mr("a", ME)], three_hours_ago),
        )
        .await
        .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), Some(ME), now());
        let tab = tabs.get(0).unwrap();

        assert_eq!(tab.all().len(), 1, "rows are on screen");
        assert_eq!(
            tab.state,
            crate::app::state::FetchState::Idle,
            "nothing has been fetched this session"
        );
        assert!(
            tab.fetched_at.is_none(),
            "and the tab must not claim it has"
        );
        assert_eq!(
            tab.cached_age().map(|d| d.as_secs()),
            Some(3 * 3600),
            "the age is what the status bar shows"
        );
    }

    /// The cache-warm render produces no notification events, so nothing
    /// may be marked as newly arrived either.
    #[tokio::test]
    async fn a_warm_render_marks_nothing_as_new() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];
        write(
            tmp.path(),
            &filters[0],
            &entry(vec![mr("a", ME), mr("b", ME)], now()),
        )
        .await
        .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), Some(ME), now());
        let tab = tabs.get(0).unwrap();

        assert!(!tab.is_new("a"));
        assert!(!tab.is_new("b"));
    }

    /// And the first live fetch after a warm start is silent too: the diff suppresses "the
    /// very first fetch of a session", which this is, however many rows the cache put on
    /// screen. Cross-restart dedup (mrq-4nw) is what handles genuinely new MRs.
    #[tokio::test]
    async fn the_first_live_fetch_after_a_warm_start_reports_nothing_as_new() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];
        write(tmp.path(), &filters[0], &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), Some(ME), now());

        let tab = tabs.get_mut(0).unwrap();
        let arrived = tab.apply_rows(vec![mr("a", ME), mr("b", ME)], std::time::Instant::now());

        assert!(arrived.is_empty(), "startup is silent: {arrived:?}");
        assert_eq!(tab.all().len(), 2, "but the rows are there");
    }

    /// A live fetch replaces the cached rows and clears the marker, or the status bar
    /// keeps saying "cached" over data that is this session's.
    #[tokio::test]
    async fn a_live_fetch_clears_the_cached_marker() {
        let tmp = dir();
        let filters = vec![filter("Assigned")];
        write(tmp.path(), &filters[0], &entry(vec![mr("a", ME)], now()))
            .await
            .unwrap();

        let mut tabs = tabs_for(&filters);
        warm(&mut tabs, &filters, tmp.path(), Some(ME), now());
        assert!(tabs.get(0).unwrap().cached_age().is_some());

        let tab = tabs.get_mut(0).unwrap();
        tab.apply_rows(vec![mr("a", ME)], std::time::Instant::now());

        assert!(tab.cached_age().is_none());
        assert_eq!(tab.state, crate::app::state::FetchState::Loaded);
    }

    /// Warming must not care that the cache directory has files for filters the user has
    /// since renamed or removed.
    #[tokio::test]
    async fn stale_files_for_removed_filters_are_ignored() {
        let tmp = dir();
        let gone = filter("Deleted");
        write(tmp.path(), &gone, &entry(vec![mr("x", ME)], now()))
            .await
            .unwrap();

        let filters = vec![filter("Assigned")];
        let mut tabs = tabs_for(&filters);

        assert_eq!(warm(&mut tabs, &filters, tmp.path(), Some(ME), now()), 0);
        assert!(tabs.get(0).unwrap().all().is_empty());
    }

    /// Cold start to first painted frame from cache, under 50 ms. The point
    /// of the cache is that the first frame does not wait on a round trip, so the budget
    /// covers reading 10 filters and rendering one of them — with no identity in scope
    /// at all, since the probe that would provide one is no longer on this path.
    #[tokio::test]
    async fn a_warm_start_loads_and_paints_within_the_frame_budget() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const FILTERS: usize = 10;
        const ROWS: usize = 100;

        let tmp = dir();
        let filters: Vec<Filter> = (0..FILTERS)
            .map(|i| filter(&format!("Filter {i}")))
            .collect();
        for f in &filters {
            let rows: Vec<MergeRequest> = (0..ROWS)
                .map(|i| mr(&format!("gid://gitlab/MergeRequest/{i}"), ME))
                .collect();
            write(tmp.path(), f, &entry(rows, now())).await.unwrap();
        }

        let mut tabs = tabs_for(&filters);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        let started = std::time::Instant::now();
        let warmed = warm(&mut tabs, &filters, tmp.path(), None, now());
        let rows = tabs.active().unwrap().all().to_vec();
        terminal
            .draw(|frame| {
                // Not the real scene — assembling one needs a Keymap, Theme and
                // ViewState, which is `run`'s job. This is the part the cache is
                // responsible for: the rows exist and are paintable with no network.
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(format!("{} rows", rows.len())),
                    frame.area(),
                );
            })
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(warmed, FILTERS);
        assert_eq!(rows.len(), ROWS);
        // Generous against the 50 ms target because this is a debug build with no
        // optimisation; a regression that matters would be orders of magnitude, not a
        // few milliseconds. mrq-ae9 measures the idle-CPU targets properly.
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "warm start took {elapsed:?} for {FILTERS} filters x {ROWS} rows"
        );
    }
}
