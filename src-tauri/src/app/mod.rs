//! Application core: state and lifecycle glue (issue #6).
//!
//! Fields for the event bus (#8) and the module manager (#9) are added in
//! their own stages.

mod state;

pub use state::AppState;
