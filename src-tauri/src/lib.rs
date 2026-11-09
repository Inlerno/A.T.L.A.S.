pub mod error;
pub mod logging;

mod app;
// TODO(#11): remove these temporary allowances once IPC commands consume the
// module manager and the event bus. Until then the MM-2 and MM-3 code has no
// consumers outside tests (agreed plan, decision 22 in #13).
#[cfg_attr(not(test), allow(dead_code))]
mod events;
#[cfg_attr(not(test), allow(dead_code))]
mod modules;

use tauri::Manager;

use crate::app::AppState;
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
            app.manage(AppState::new(log_guard, EventBus::new(DEFAULT_CAPACITY)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| match event {
        tauri::RunEvent::ExitRequested { .. } => {
            tracing::info!("exit requested");
        }
        tauri::RunEvent::Exit => {
            tracing::info!("ATLAS exiting");
            // `App::run` ends the process with `std::process::exit`, so managed
            // state is never dropped: close the event bus and flush the log
            // file explicitly, in this order, so that the closing is logged.
            if let Some(state) = app_handle.try_state::<AppState>() {
                state.close_events();
                state.flush_logs();
            }
        }
        _ => {}
    });
}
