//! Global logging initialisation with a writable log directory (issue #6).
//!
//! `logging::init` installs a process-global subscriber, so this file is its
//! own test binary (separate process) with exactly one test.

use std::fs;
use std::path::PathBuf;

use atlas_lib::logging::{self, LOG_FILE_PREFIX, LOG_FILE_SUFFIX};

fn unique_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "atlas-it-init-file-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn init_writes_to_rolling_log_file() {
    let dir = unique_temp_dir();
    let log_dir = dir.join("logs");

    let guard = logging::init(Ok(log_dir.clone()), Some("info"))
        .expect("init succeeds")
        .expect("file logging is enabled");
    tracing::info!("atlas-init-file-test-record");
    tracing::debug!("atlas-filtered-out-debug-record");
    drop(guard); // flush

    let content: String = fs::read_dir(&log_dir)
        .expect("log dir exists")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(LOG_FILE_PREFIX) && name.ends_with(LOG_FILE_SUFFIX)
        })
        .map(|e| fs::read_to_string(e.path()).unwrap_or_default())
        .collect();

    assert!(content.contains("file logging enabled"), "{content}");
    assert!(content.contains("atlas-init-file-test-record"), "{content}");
    assert!(
        !content.contains("atlas-filtered-out-debug-record"),
        "{content}"
    );
    assert!(
        !content.contains('\u{1b}'),
        "no ANSI escapes in the log file"
    );

    let _ = fs::remove_dir_all(&dir);
}
