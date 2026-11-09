//! Panic hook test (issue #6).
//!
//! The panic hook is process-global. This file is compiled into its own test
//! binary, which cargo runs as a separate process, and it contains exactly one
//! test, so no other test can run in parallel with the hook installed.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

static PREVIOUS_HOOK_CALLED: AtomicBool = AtomicBool::new(false);

#[test]
fn panic_is_logged_and_previous_hook_still_runs() {
    // Stand-in for the default hook, to prove that it is still called.
    std::panic::set_hook(Box::new(|_| {
        PREVIOUS_HOOK_CALLED.store(true, Ordering::SeqCst);
    }));
    atlas_lib::logging::install_panic_hook();

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        std::panic::catch_unwind(|| panic!("atlas-panic-hook-test"))
    });

    // Restore the default hook before asserting.
    drop(std::panic::take_hook());

    assert!(result.is_err(), "the closure must panic");
    let logged = String::from_utf8(capture.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
        .expect("utf-8 log");
    assert!(logged.contains("atlas-panic-hook-test"), "log: {logged}");
    assert!(logged.contains("ERROR"), "log: {logged}");
    assert!(logged.contains("panic"), "log: {logged}");
    assert!(
        PREVIOUS_HOOK_CALLED.load(Ordering::SeqCst),
        "the previously installed hook must still run"
    );
}
