use std::sync::{Mutex, MutexGuard, PoisonError};

use tracing_appender::non_blocking::WorkerGuard;

use crate::events::bus::EventBus;
use crate::modules::manager::ModuleManagerHandle;

/// State managed by Tauri (`app.manage`).
///
/// Holds the log writer guard, the central event bus and the module manager.
/// `tauri::App::run` ends the process with `std::process::exit`, so
/// destructors of managed state do not run. The exit steps are therefore
/// explicit (decision 28 in #13): stop the modules, then close the bus
/// ([`AppState::close_events`]), then release the guard
/// ([`AppState::flush_logs`]), which flushes buffered log records.
pub struct AppState {
    log_guard: Mutex<Option<WorkerGuard>>,
    events: EventBus,
    modules: ModuleManagerHandle,
    exit: Mutex<ExitPhase>,
}

/// Where the application is on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitPhase {
    Running,
    StoppingModules,
    ModulesStopped,
    /// `RunEvent::Exit` has begun: the process ends right after it.
    Exiting,
}

/// What to do with an exit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStep {
    /// First request: prevent the exit and stop the modules, then exit again.
    StopModules,
    /// The modules are still stopping: keep preventing the exit.
    Wait,
    /// The modules are stopped: let the application exit (no loop).
    Exit,
}

impl AppState {
    /// `log_guard` is `None` when file logging is unavailable (stdout fallback).
    pub fn new(
        log_guard: Option<WorkerGuard>,
        events: EventBus,
        modules: ModuleManagerHandle,
    ) -> Self {
        Self {
            log_guard: Mutex::new(log_guard),
            events,
            modules,
            exit: Mutex::new(ExitPhase::Running),
        }
    }

    pub fn modules(&self) -> &ModuleManagerHandle {
        &self.modules
    }

    /// Decides what to do with an exit request. Exactly one caller gets
    /// [`ExitStep::StopModules`]; a request after the modules have stopped
    /// gets [`ExitStep::Exit`], so a second exit does not start a loop.
    pub fn request_exit(&self) -> ExitStep {
        let mut phase = lock(&self.exit);
        match *phase {
            ExitPhase::Running => {
                *phase = ExitPhase::StoppingModules;
                ExitStep::StopModules
            }
            ExitPhase::StoppingModules => ExitStep::Wait,
            ExitPhase::ModulesStopped | ExitPhase::Exiting => ExitStep::Exit,
        }
    }

    /// Records that the modules have stopped (or were terminated). Returns
    /// `true` if the exit must now be requested again, `false` if
    /// `RunEvent::Exit` has already begun: requesting an exit then could end
    /// the process before the logs are flushed.
    pub fn modules_stopped(&self) -> bool {
        let mut phase = lock(&self.exit);
        if *phase == ExitPhase::Exiting {
            return false;
        }
        *phase = ExitPhase::ModulesStopped;
        true
    }

    /// Called at `RunEvent::Exit`. Returns `true` if the modules still have to
    /// be stopped there, because the exit could not be postponed (e.g. on a
    /// restart).
    pub fn begin_exit(&self) -> bool {
        let mut phase = lock(&self.exit);
        let pending = *phase != ExitPhase::ModulesStopped;
        *phase = ExitPhase::Exiting;
        pending
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

/// The exit phase stays meaningful if a holder panicked: recover the lock.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::events::bus::{BusClosed, DEFAULT_CAPACITY};
    use crate::events::types::{AppEvent, ModuleEvent};
    use crate::modules::contract::{ModuleId, ModuleStatus};
    use crate::modules::manager;

    /// A state with a module manager whose actor never runs: these tests do
    /// not send it commands.
    fn state_with(log_guard: Option<WorkerGuard>, events: EventBus) -> AppState {
        let (modules, _actor) = manager::create(Vec::new(), events.clone());
        AppState::new(log_guard, events, modules)
    }

    fn sink_guard() -> WorkerGuard {
        let (_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        guard
    }

    fn state(log_guard: Option<WorkerGuard>) -> AppState {
        state_with(log_guard, EventBus::new(DEFAULT_CAPACITY))
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
        let state = state_with(None, bus.clone());
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

    #[test]
    fn exit_requests_stop_modules_once_and_then_let_the_exit_pass() {
        let state = state(None);
        assert_eq!(state.request_exit(), ExitStep::StopModules);
        assert_eq!(state.request_exit(), ExitStep::Wait);
        assert_eq!(state.request_exit(), ExitStep::Wait);
        assert!(state.modules_stopped(), "request the exit again");
        assert_eq!(state.request_exit(), ExitStep::Exit);
        assert_eq!(
            state.request_exit(),
            ExitStep::Exit,
            "no loop: every later request passes"
        );
        assert!(!state.begin_exit(), "the modules are already stopped");
    }

    #[test]
    fn an_exit_that_could_not_wait_stops_the_modules_itself() {
        // E.g. a restart: `RunEvent::Exit` arrives while the modules stop.
        let state = state(None);
        assert_eq!(state.request_exit(), ExitStep::StopModules);
        assert!(state.begin_exit(), "Exit must stop the modules itself");
        assert!(
            !state.modules_stopped(),
            "the background stop must not request another exit"
        );
        assert_eq!(state.request_exit(), ExitStep::Exit);

        let untouched = state_with(None, EventBus::new(DEFAULT_CAPACITY));
        assert!(untouched.begin_exit(), "no exit request at all");
    }

    #[test]
    fn concurrent_exit_requests_start_the_stop_exactly_once() {
        let state = Arc::new(state(None));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                std::thread::spawn(move || state.request_exit())
            })
            .collect();
        let steps: Vec<ExitStep> = handles
            .into_iter()
            .map(|handle| handle.join().expect("exit thread"))
            .collect();
        let starts = steps
            .iter()
            .filter(|step| **step == ExitStep::StopModules)
            .count();
        assert_eq!(starts, 1);
        assert!(
            steps
                .iter()
                .all(|step| matches!(step, ExitStep::StopModules | ExitStep::Wait))
        );
    }
}
