//! Structured logging (issue #6).
//!
//! - `tracing` events go to stdout and to a daily rolling file in the app log
//!   directory (`%LOCALAPPDATA%\com.inlerno.atlas\logs` on Windows).
//! - At most [`MAX_LOG_FILES`] log files are kept. `tracing-appender` deletes the
//!   oldest files when the appender is created (app start) and on every daily
//!   rollover.
//! - If the file sink cannot be created, logging falls back to stdout only and a
//!   warning is logged; the application keeps running.
//! - `log` records (e.g. from Tauri itself) are bridged into `tracing`.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::AppError;

/// File name prefix of log files: `atlas.YYYY-MM-DD.log`.
pub const LOG_FILE_PREFIX: &str = "atlas";
/// File name suffix (extension) of log files.
pub const LOG_FILE_SUFFIX: &str = "log";
/// Number of log files kept, including the current one. Older files are deleted.
pub const MAX_LOG_FILES: usize = 7;
/// Filter used when `RUST_LOG` is not set, empty or invalid.
pub const DEFAULT_FILTER: &str = "info";

/// Builds the level filter from `RUST_LOG`-style directives.
///
/// Falls back to [`DEFAULT_FILTER`] when the directives are missing, blank or
/// invalid. Takes the value as a parameter so that tests never mutate the
/// process environment (`std::env::set_var` is `unsafe` in Rust 2024).
pub fn env_filter(directives: Option<&str>) -> EnvFilter {
    directives
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .and_then(|d| EnvFilter::try_new(d).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER))
}

/// Creates the daily rolling file appender in `dir`, keeping at most
/// [`MAX_LOG_FILES`] files (older ones are deleted on creation and on rollover).
pub fn rolling_appender(dir: &Path) -> Result<RollingFileAppender, AppError> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(dir)?;
    Ok(appender)
}

/// Creates the non-blocking file writer for `dir`.
///
/// The returned [`WorkerGuard`] flushes buffered records when dropped; it must
/// be kept alive for the whole application lifetime and dropped on exit.
pub fn file_sink(dir: &Path) -> Result<(NonBlocking, WorkerGuard), AppError> {
    Ok(tracing_appender::non_blocking(rolling_appender(dir)?))
}

/// Installs the global subscriber.
///
/// Returns the file writer guard, or `None` when file logging is unavailable
/// (fallback to stdout only, with a warning). Fails only if a global
/// subscriber is already installed.
pub fn init(
    log_dir: Result<PathBuf, AppError>,
    directives: Option<&str>,
) -> Result<Option<WorkerGuard>, AppError> {
    let stdout_layer = fmt::layer().with_writer(std::io::stdout);

    match log_dir.and_then(|dir| file_sink(&dir).map(|(writer, guard)| (dir, writer, guard))) {
        Ok((dir, writer, guard)) => {
            tracing_subscriber::registry()
                .with(env_filter(directives))
                .with(stdout_layer)
                .with(fmt::layer().with_ansi(false).with_writer(writer))
                .try_init()?;
            tracing::info!(
                log_dir = %dir.display(),
                max_files = MAX_LOG_FILES,
                "file logging enabled"
            );
            Ok(Some(guard))
        }
        Err(error) => {
            tracing_subscriber::registry()
                .with(env_filter(directives))
                .with(stdout_layer)
                .try_init()?;
            tracing::warn!(%error, "file logging unavailable, logging to stdout only");
            Ok(None)
        }
    }
}

/// Installs a panic hook that records panics in the log and then calls the
/// previously installed hook (standard behaviour is preserved).
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "panic", "{info}");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "atlas-test-{tag}-{}-{n}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn log_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(LOG_FILE_PREFIX) && n.ends_with(LOG_FILE_SUFFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn env_filter_defaults_to_info() {
        assert_eq!(env_filter(None).to_string(), DEFAULT_FILTER);
        assert_eq!(env_filter(Some("")).to_string(), DEFAULT_FILTER);
        assert_eq!(env_filter(Some("   ")).to_string(), DEFAULT_FILTER);
    }

    #[test]
    fn env_filter_uses_directives() {
        assert_eq!(env_filter(Some("debug")).to_string(), "debug");
        assert_eq!(env_filter(Some(" atlas=trace ")).to_string(), "atlas=trace");
    }

    #[test]
    fn env_filter_falls_back_on_invalid_directives() {
        assert_eq!(
            env_filter(Some("atlas=not-a-level")).to_string(),
            DEFAULT_FILTER
        );
    }

    #[test]
    fn rolling_appender_keeps_at_most_max_log_files() {
        let dir = unique_temp_dir("prune");
        // Ten old log files (dates far from "today" so the current file is distinct).
        let old: Vec<String> = (1..=10)
            .map(|day| format!("{LOG_FILE_PREFIX}.2025-01-{day:02}.{LOG_FILE_SUFFIX}"))
            .collect();
        for name in &old {
            fs::write(dir.join(name), b"old\n").expect("write old log");
        }
        // Unrelated files must never be deleted.
        fs::write(dir.join("notes.txt"), b"keep").expect("write unrelated");
        fs::write(dir.join("atlas-settings.json"), b"keep").expect("write unrelated");

        let appender = rolling_appender(&dir).expect("appender");

        let remaining = log_files(&dir);
        assert_eq!(remaining.len(), MAX_LOG_FILES, "remaining: {remaining:?}");
        let current: Vec<&String> = remaining.iter().filter(|n| !old.contains(n)).collect();
        assert_eq!(
            current.len(),
            1,
            "exactly one current log file: {remaining:?}"
        );
        assert!(dir.join("notes.txt").exists());
        assert!(dir.join("atlas-settings.json").exists());

        drop(appender);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rolling_appender_does_not_prune_below_limit() {
        let dir = unique_temp_dir("noprune");
        for day in 1..=3 {
            fs::write(
                dir.join(format!(
                    "{LOG_FILE_PREFIX}.2025-02-{day:02}.{LOG_FILE_SUFFIX}"
                )),
                b"old\n",
            )
            .expect("write old log");
        }
        let appender = rolling_appender(&dir).expect("appender");
        assert_eq!(log_files(&dir).len(), 4);
        drop(appender);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_sink_writes_records_and_flushes_on_guard_drop() {
        let dir = unique_temp_dir("sink");
        let (writer, guard) = file_sink(&dir).expect("file sink");
        let subscriber = tracing_subscriber::registry()
            .with(env_filter(None))
            .with(fmt::layer().with_ansi(false).with_writer(writer));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("atlas-file-sink-test-record");
        });
        drop(guard);

        let content: String = log_files(&dir)
            .iter()
            .map(|n| fs::read_to_string(dir.join(n)).unwrap_or_default())
            .collect();
        assert!(content.contains("atlas-file-sink-test-record"), "{content}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_sink_fails_when_log_dir_is_a_file() {
        let dir = unique_temp_dir("notadir");
        let file_path = dir.join("not-a-directory");
        fs::write(&file_path, b"x").expect("write file");
        let result = file_sink(&file_path);
        assert!(matches!(result, Err(AppError::Io(_))), "{:?}", result.err());
        let _ = fs::remove_dir_all(&dir);
    }
}
