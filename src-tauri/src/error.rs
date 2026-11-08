//! Application error model (issue #6).
//!
//! Only variants that are actually produced by the code exist here; new
//! variants are added together with the code that creates them. The IPC
//! representation (`ErrorDto`) is intentionally not part of this stage (#11).

use thiserror::Error;

/// Internal application error.
///
/// Messages must never contain secrets. They may contain local paths and are
/// therefore meant for logs, not for the UI.
#[derive(Debug, Error)]
pub enum AppError {
    /// An application directory (e.g. the log directory) could not be resolved.
    #[error("could not resolve application directory: {0}")]
    Path(#[from] tauri::Error),

    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The rolling log file appender could not be created.
    #[error("could not create the log file appender: {0}")]
    LogFile(#[from] tracing_appender::rolling::InitError),

    /// The global tracing subscriber could not be installed.
    #[error("could not install the global logger: {0}")]
    LoggerInit(#[from] tracing_subscriber::util::TryInitError),
}
