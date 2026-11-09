use std::sync::Mutex;

use tracing_appender::non_blocking::WorkerGuard;

use crate::events::bus::EventBus;

/// State managed by Tauri (`app.manage`).
///
/// Holds the log writer guard and the central event bus. `tauri::App::run`
/// ends the process with `std::process::exit`, so destructors of managed state
/// do not run. On `RunEvent::Exit` the bus is therefore closed explicitly
/// ([`AppState::close_events`]) and then the guard is released
/// ([`AppState::flush_logs`]), which flushes buffered log records.
pub struct AppState {
    log_guard: Mutex<Option<WorkerGuard>>,
    events: EventBus,
}

impl AppState {
    /// `log_guard` is `None` when file logging is unavailable (stdout fallback).
    pub fn new(log_guard: Option<WorkerGuard>, events: EventBus) -> Self {
        Self {
            log_guard: Mutex::new(log_guard),
            events,
        }
    }

    /// Closes the event bus: subscribers receive the events published before,
    /// then `Closed`; later publications are rejected. Idempotent.
    pub fn close_events(&self) {
        self.events.close();
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

    use crate::events::bus::{BusClosed, DEFAULT_CAPACITY};
    use crate::events::types::{AppEvent, ModuleEvent};
    use crate::modules::contract::{ModuleId, ModuleStatus};

    fn sink_guard() -> WorkerGuard {
        let (_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        guard
    }

    fn state(log_guard: Option<WorkerGuard>) -> AppState {
        AppState::new(log_guard, EventBus::new(DEFAULT_CAPACITY))
    }

    #[test]
    fn flush_logs_is_idempotent() {
        let state = state(Some(sink_guard()));
        assert!(state.has_log_guard());
        state.flush_logs();
        assert!(!state.has_log_guard());
        state.flush_logs();
        state.flush_logs();
        assert!(!state.has_log_guard());
    }

    #[test]
    fn flush_logs_without_file_logging_is_a_no_op() {
        let state = state(None);
        state.flush_logs();
        state.flush_logs();
        assert!(!state.has_log_guard());
    }

    #[test]
    fn flush_logs_recovers_from_poisoned_mutex() {
        let state = Arc::new(state(Some(sink_guard())));
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
        let state = Arc::new(state(Some(sink_guard())));
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

    #[test]
    fn close_events_closes_the_shared_bus_and_is_idempotent() {
        let bus = EventBus::new(DEFAULT_CAPACITY);
        let state = AppState::new(None, bus.clone());
        let event = || {
            AppEvent::Module(ModuleEvent::StatusChanged {
                id: ModuleId::new("t.a").expect("valid module id"),
                previous: ModuleStatus::default(),
                current: ModuleStatus::default(),
            })
        };
        assert_eq!(bus.publish(event()), Ok(1));

        state.close_events();
        state.close_events();
        assert_eq!(bus.publish(event()), Err(BusClosed));
    }
}
