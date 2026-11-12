pub mod error;
pub mod logging;

mod app;
mod clock;
// TODO(#11): remove these temporary allowances once IPC commands consume the
// module manager and the event bus. Until then parts of the MM-2..MM-4 code
// have no consumers outside tests (agreed plan, decision 22 in #13).
#[cfg_attr(not(test), allow(dead_code))]
mod events;
#[cfg_attr(not(test), allow(dead_code))]
mod modules;

use tauri::Manager;

use crate::app::{AppState, ExitStep};
use crate::error::AppError;
use crate::events::bus::{DEFAULT_CAPACITY, EventBus};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            // Must not fail: an error returned from setup makes Tauri panic.
            // File logging problems fall back to stdout inside `logging::init`.
            let log_dir = app.path().app_log_dir().map_err(AppError::from);
            let rust_log = std::env::var("RUST_LOG").ok();
            let log_guard = match logging::init(log_dir, rust_log.as_deref()) {
                Ok(guard) => guard,
                Err(error) => {
                    eprintln!("ATLAS: logging initialisation failed: {error}");
                    None
                }
            };
            logging::install_panic_hook();
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "ATLAS starting");
            let events = EventBus::new(DEFAULT_CAPACITY);
            // The manager loads the module configuration and starts the
            // enabled modules on its own task (#10, ADR-015). The store keeps
            // the configuration in memory for this run; persistence between
            // runs arrives with MM-5B.
            let store = Box::new(modules::config::InMemoryConfigStore::default());
            let (modules, manager) =
                modules::manager::create(modules::catalog(), store, events.clone());
            tauri::async_runtime::spawn(manager);
            app.manage(AppState::new(log_guard, events, modules));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Exit order (decision 28 in #13): stop the modules, wait for their stops
    // and `StatusChanged` events (bounded by the shutdown timeout), close the
    // event bus, flush the logs, exit. `App::run` ends the process with
    // `std::process::exit`, so managed state is never dropped and each step is
    // explicit.
    app.run(|app_handle, event| match event {
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            let Some(state) = app_handle.try_state::<AppState>() else {
                return;
            };
            match state.request_exit() {
                ExitStep::StopModules => {
                    // Keep the process alive while the modules stop off the
                    // main thread, then exit again; that second request passes.
                    api.prevent_exit();
                    tracing::info!("exit requested: stopping modules");
                    let modules = state.modules().clone();
                    let handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = modules.shutdown().await;
                        // Not if `RunEvent::Exit` has already begun: an exit
                        // requested then could end the process before the
                        // logs are flushed.
                        let exit_again = handle
                            .try_state::<AppState>()
                            .is_none_or(|state| state.modules_stopped());
                        if exit_again {
                            handle.exit(code.unwrap_or(0));
                        }
                    });
                }
                ExitStep::Wait => api.prevent_exit(),
                ExitStep::Exit => tracing::info!("exit requested"),
            }
        }
        tauri::RunEvent::Exit => {
            tracing::info!("ATLAS exiting");
            if let Some(state) = app_handle.try_state::<AppState>() {
                if state.begin_exit() {
                    // Exit without a completed stop (e.g. a restart, where the
                    // exit cannot be prevented): stop the modules here, within
                    // the same overall timeout.
                    let _ = tauri::async_runtime::block_on(state.modules().shutdown());
                }
                state.close_events();
                state.flush_logs();
            }
        }
        _ => {}
    });
}
