//! Module Manager.
//!
//! Issue #7 (MM-2): pure data and logic — module identifiers and statuses,
//! the dependency graph and two-phase change plans (ADR-011, ADR-012).
//! Issue #9 (MM-4): the module contract, the context given to instances and
//! the lifecycle manager that applies plans (ADR-014).
//! Issue #10 (MM-5A): module configuration — the user's decision and settings,
//! their store, loading at startup and the startup of enabled modules
//! (ADR-015). Status changes are
//! published only through the central typed event bus (#8, ADR-013). Modules
//! are compile-time Rust code; there is no dynamic (DLL) loading.

pub mod config;
pub mod context;
pub mod contract;
pub mod graph;
pub mod manager;
pub mod plan;

#[cfg(test)]
mod manager_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
pub(crate) mod testing;

use contract::ModuleFactory;

/// Modules compiled into this build. Empty for now: demo modules arrive with
/// #12 and real modules later.
pub fn catalog() -> Vec<Box<dyn ModuleFactory>> {
    Vec::new()
}
