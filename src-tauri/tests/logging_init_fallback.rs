//! Global logging initialisation when file logging is unavailable (issue #6).
//!
//! `logging::init` installs a process-global subscriber, so this file is its
//! own test binary (separate process) with exactly one test.

use std::fs;

use atlas_lib::error::AppError;
use atlas_lib::logging;

#[test]
fn init_falls_back_to_stdout_and_never_fails_on_file_errors() {
    let dir = std::env::temp_dir().join(format!(
        "atlas-it-init-fallback-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    // The "log directory" is an existing regular file: the file sink cannot be created.
    let not_a_dir = dir.join("not-a-directory");
    fs::write(&not_a_dir, b"x").expect("write file");

    let guard = logging::init(Ok(not_a_dir), None).expect("fallback must not fail");
    assert!(
        guard.is_none(),
        "file logging must be disabled in fallback mode"
    );
    tracing::info!("logging still works in fallback mode");

    // A second global initialisation is the only error `init` reports.
    let second = logging::init(Err(AppError::Io(std::io::Error::other("unresolved"))), None);
    assert!(
        matches!(second, Err(AppError::LoggerInit(_))),
        "second init must report LoggerInit, got {:?}",
        second.err()
    );

    let _ = fs::remove_dir_all(&dir);
}
