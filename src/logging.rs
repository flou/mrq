//! File logging and the in-memory ring buffer behind the log popup.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt as fmt_layer};

/// How many lines the log popup can show.
pub const RING_CAPACITY: usize = 50;

/// The last few log lines, shared with the UI.
///
/// Cheap to clone; the UI holds one and the layer holds another.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, line: String) {
        // A poisoned lock means a previous writer panicked mid-push. Losing log lines is
        // strictly better than panicking inside the logger, which would turn a
        // recoverable bug into a crash during the panic report.
        let Ok(mut lines) = self.lines.lock() else {
            return;
        };
        if lines.len() == RING_CAPACITY {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Seed the ring directly, for tests elsewhere in the crate that need a populated
    /// buffer without installing a global subscriber.
    #[cfg(test)]
    pub(crate) fn seed(&self, lines: &[&str]) {
        for line in lines {
            self.push((*line).to_owned());
        }
    }

    /// The retained lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.lines.lock().map(|l| l.len()).unwrap_or(0)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Collects an event's message and fields into one line.
struct LineVisitor {
    message: String,
    fields: String,
}

impl LineVisitor {
    const fn new() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
        }
    }
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

/// Feeds formatted lines into the ring buffer.
struct RingLayer {
    buffer: LogBuffer,
}

impl<S> Layer<S> for RingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LineVisitor::new();
        event.record(&mut visitor);

        let meta = event.metadata();
        // No timestamp: the popup is narrow, the lines are recent by construction, and
        // the file has full timestamps for anything that needs correlating.
        self.buffer.push(format!(
            "{:>5} {}: {}{}",
            meta.level(),
            meta.target(),
            visitor.message,
            visitor.fields
        ));
    }
}

/// Keeps the non-blocking writer's worker thread alive.
///
/// Dropping this flushes and stops the appender, so it must outlive the TUI or the last
/// lines before a crash — the ones that explain it — are lost.
pub struct LogHandle {
    _appender: Option<tracing_appender::non_blocking::WorkerGuard>,
    path: Option<PathBuf>,
    buffer: LogBuffer,
}

impl LogHandle {
    /// The buffer the log popup renders.
    pub fn buffer(&self) -> LogBuffer {
        self.buffer.clone()
    }

    /// Where lines are being written.
    #[cfg(test)]
    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl fmt::Debug for LogHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogHandle")
            .field("path", &self.path)
            .field("buffered_lines", &self.buffer.len())
            .finish()
    }
}

/// Build the filter from the explicit level, then `$RUST_LOG`, then a default.
///
/// The flag wins over the environment so that `--log-level debug` does what it says even
/// in a shell where someone exported `RUST_LOG=warn` months ago.
///
/// The level applies to `mrq` only; dependencies stay at `warn`. A global `debug` fills
/// the log with HTTP/2 frame traffic from hyper — hundreds of lines per request — which
/// would leave the 50-line log popup showing nothing but connection internals, the exact
/// opposite of what it is for. `$RUST_LOG` still overrides everything for the rare case
/// where the dependency chatter is what you need.
fn env_filter(level: Option<Level>) -> EnvFilter {
    if let Some(level) = level {
        let level = level.as_str().to_ascii_lowercase();
        return EnvFilter::new(format!("warn,{}={level}", env!("CARGO_CRATE_NAME")));
    }
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,{}=info", env!("CARGO_CRATE_NAME"))))
}

/// Parse a `--log-level` value.
pub fn parse_level(raw: &str) -> Option<Level> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(Level::TRACE),
        "debug" => Some(Level::DEBUG),
        "info" => Some(Level::INFO),
        "warn" | "warning" => Some(Level::WARN),
        "error" => Some(Level::ERROR),
        _ => None,
    }
}

/// The default log file inside the state directory.
pub fn default_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("mrq.log")
}

/// Install the subscriber.
///
/// `path` is the file to append to; `None` installs only the ring buffer, which is what
/// `mrq check` and the tests want. Returns a handle that must be kept alive.
pub fn init(path: Option<&Path>, level: Option<Level>) -> LogHandle {
    let buffer = LogBuffer::new();
    let ring = RingLayer {
        buffer: buffer.clone(),
    };

    let (file_layer, appender, resolved_path) = match path.and_then(open_log_file) {
        Some((file, path)) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let layer = fmt_layer::layer()
                .with_writer(writer)
                // No colour: this is a file, and escape codes make `grep` output
                // unreadable.
                .with_ansi(false)
                .with_target(true);
            (Some(layer), Some(guard), Some(path))
        }
        None => (None, None, None),
    };

    // A failure to install is not fatal. Losing logs is bad; refusing to start because
    // the state directory is read-only is worse, and it is the caller's own second
    // `init` in a test that usually trips this.
    let _ = tracing_subscriber::registry()
        .with(env_filter(level))
        .with(ring)
        .with(file_layer)
        .try_init();

    LogHandle {
        _appender: appender,
        path: resolved_path,
        buffer,
    }
}

fn open_log_file(path: &Path) -> Option<(std::fs::File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        // The log records project paths, branch names and MR titles from private
        // repositories, so it is owner-only like the cache and state directories.
        .mode(0o600)
        .open(path)
        .ok()?;
    Some((file, path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the ring layer directly. `init` installs a process-global subscriber, so
    /// tests that need isolation build the layer themselves instead.
    fn with_ring<F: FnOnce()>(f: F) -> Vec<String> {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("trace"))
            .with(RingLayer {
                buffer: buffer.clone(),
            });
        tracing::subscriber::with_default(subscriber, f);
        buffer.lines()
    }

    #[test]
    fn events_reach_the_ring_buffer_with_level_and_target() {
        let lines = with_ring(|| {
            tracing::info!("hello");
            tracing::warn!(count = 3, "careful");
        });

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("INFO"), "{:?}", lines[0]);
        assert!(lines[0].contains("hello"), "{:?}", lines[0]);
        assert!(lines[1].contains("WARN"), "{:?}", lines[1]);
        assert!(lines[1].contains("count=3"), "{:?}", lines[1]);
    }

    #[test]
    fn the_ring_buffer_is_bounded_and_keeps_the_newest() {
        let lines = with_ring(|| {
            for i in 0..(RING_CAPACITY + 25) {
                tracing::info!("line {i}");
            }
        });

        assert_eq!(lines.len(), RING_CAPACITY);
        assert!(
            lines.first().unwrap().contains("line 25"),
            "oldest retained line should be 25, got {:?}",
            lines.first()
        );
        assert!(
            lines
                .last()
                .unwrap()
                .contains(&format!("line {}", RING_CAPACITY + 24)),
            "{:?}",
            lines.last()
        );
    }

    #[test]
    fn a_debug_logged_config_never_contains_the_token() {
        use crate::config::schema::Gitlab;
        use crate::config::token::{TokenEnv, resolve};

        let secret = "glpat-DO-NOT-LOG-ME";
        let gitlab = Gitlab {
            token: Some(secret.into()),
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;

        #[derive(Debug)]
        struct Holder {
            token: crate::config::token::Token,
        }
        let holder = Holder { token };

        let lines = with_ring(|| {
            tracing::debug!(?holder, "starting up");
            tracing::info!("token is {}", holder.token);
            tracing::error!(token = ?holder.token, "auth failed");
        });

        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(!line.contains(secret), "token leaked into the log: {line}");
            assert!(line.contains("redacted"), "{line}");
        }
    }

    #[test]
    fn log_levels_parse_and_reject_nonsense() {
        assert_eq!(parse_level("debug"), Some(Level::DEBUG));
        assert_eq!(parse_level("  TRACE "), Some(Level::TRACE));
        assert_eq!(parse_level("warning"), Some(Level::WARN), "common spelling");
        assert_eq!(parse_level("error"), Some(Level::ERROR));
        assert_eq!(parse_level("verbose"), None);
        assert_eq!(parse_level(""), None);
    }

    #[test]
    fn an_explicit_level_beats_the_environment() {
        let explicit = env_filter(Some(Level::DEBUG)).to_string();
        assert!(explicit.contains("debug"), "{explicit}");
    }

    #[test]
    fn the_level_applies_to_mrq_and_not_to_dependencies() {
        for level in [Level::DEBUG, Level::TRACE] {
            let filter = env_filter(Some(level)).to_string();
            assert!(
                filter.contains("warn"),
                "dependencies should stay quiet: {filter}"
            );
            assert!(
                filter.contains(env!("CARGO_CRATE_NAME")),
                "the level should be scoped to mrq: {filter}"
            );
        }

        let default = env_filter(None).to_string();
        assert!(default.contains("warn"), "{default}");
    }

    #[test]
    fn the_default_log_file_lives_in_the_state_dir() {
        let path = default_log_path(Path::new("/home/u/.local/state/mrq"));
        assert_eq!(path, Path::new("/home/u/.local/state/mrq/mrq.log"));
    }

    #[test]
    fn the_log_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/mrq.log");

        let (_file, created) = open_log_file(&path).expect("should create the log file");
        assert_eq!(created, path);
        assert!(path.is_file(), "parent directories are created on demand");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log should not be world-readable");
    }

    #[test]
    fn an_unwritable_log_path_is_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("is-a-directory");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(open_log_file(&dir).is_none());

        let handle = init(Some(&dir), Some(Level::INFO));
        assert!(handle.path().is_none(), "no file, but still running");
        assert!(handle.buffer().is_empty());
    }

    #[test]
    fn the_handle_debug_does_not_dump_log_contents() {
        let handle = init(None, Some(Level::INFO));
        let rendered = format!("{handle:?}");
        assert!(rendered.contains("LogHandle"), "{rendered}");
        assert!(rendered.contains("buffered_lines"), "{rendered}");
    }

    #[test]
    fn buffer_clones_share_one_ring() {
        let buffer = LogBuffer::new();
        let clone = buffer.clone();
        buffer.push("one".into());
        clone.push("two".into());

        assert_eq!(buffer.lines(), ["one", "two"]);
        assert_eq!(clone.len(), 2);
    }
}
