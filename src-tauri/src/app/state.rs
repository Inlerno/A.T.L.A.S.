use std::sync::Mutex;

use tracing_appender::non_blocking::WorkerGuard;

/// State managed by Tauri (`app.manage`).
///
/// Holds the log writer guard. `tauri::App::run` ends the process with
/// `std::process::exit`, so destructors of managed state do not run; the guard
/// is therefore released explicitly in [`AppState::flush_logs`] on
/// `RunEvent::Exit`, which flushes buffered log records.
pub struct AppState {
    log_guard: Mutex<Option<WorkerGuard>>,
}

impl AppState {
    /// `log_guard` is `None` when file logging is unavailable (stdout fallback).
    pub fn new(log_guard: Option<WorkerGuard>) -> Self {
        Self {
            log_guard: Mutex::new(log_guard),
        }
    }

    /// Flushes buffered log records to the log file by dropping the writer guard.
    ///
    /// Safe to call any number of times: only the first call drops the guard,
    /// later calls are no-ops. A poisoned mutex is recovered rather than
    /// propagated, so flushing never panics.
    pub fn flush_logs(&self) {
        let guard = match self.log_guard.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(guard);
    }

    #[cfg(test)]
    fn has_log_guard(&self) -> bool {
        match self.log_guard.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sink_guard() -> WorkerGuard {
        let (_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        guard
    }

    #[test]
    fn flush_logs_is_idempotent() {
        let state = AppState::new(Some(sink_guard()));
        assert!(state.has_log_guard());
        state.flush_logs();
        assert!(!state.has_log_guard());
        state.flush_logs();
        state.flush_logs();
        assert!(!state.has_log_guard());
    }

    #[test]
    fn flush_logs_without_file_logging_is_a_no_op() {
        let state = AppState::new(None);
        state.flush_logs();
        state.flush_logs();
        assert!(!state.has_log_guard());
    }

    #[test]
    fn flush_logs_recovers_from_poisoned_mutex() {
        let state = Arc::new(AppState::new(Some(sink_guard())));
        let poisoner = Arc::clone(&state);
        let result = std::thread::spawn(move || {
            let _held = poisoner.log_guard.lock().expect("lock");
            panic!("poison the mutex");
        })
        .join();
        assert!(result.is_err(), "helper thread must panic");
        assert!(state.log_guard.is_poisoned());

        state.flush_logs();
        state.flush_logs();
        assert!(!state.has_log_guard());
    }

    #[test]
    fn flush_logs_is_safe_from_concurrent_callers() {
        let state = Arc::new(AppState::new(Some(sink_guard())));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                std::thread::spawn(move || state.flush_logs())
            })
            .collect();
        for handle in handles {
            handle.join().expect("flush thread");
        }
        assert!(!state.has_log_guard());
    }
}
