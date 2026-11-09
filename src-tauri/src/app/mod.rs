//! Application core: state and lifecycle glue (issue #6).
//!
//! Holds the central event bus (#8) and the module manager (#9), and orders
//! the exit steps.

mod state;

pub use state::{AppState, ExitStep};
