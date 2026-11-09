//! Application core: state and lifecycle glue (issue #6).
//!
//! Holds the central event bus (#8); the module manager (#9) is added in its
//! own stage.

mod state;

pub use state::AppState;
