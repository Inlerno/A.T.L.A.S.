//! Module Manager.
//!
//! Issue #7 (MM-2): pure data and logic — module identifiers and statuses,
//! the dependency graph and two-phase change plans (ADR-011, ADR-012).
//! Issue #9 (MM-4): the module contract, the context given to instances and
//! the lifecycle manager that applies plans (ADR-014).
//! Issue #10 (MM-5A): module configuration — the user's decision and settings,
//! their store, loading at startup and the startup of enabled modules
//! (ADR-015).
//! Issue #12 (MM-7): demo modules behind the feature `demo-modules`.
//! Status changes are
//! published only through the central typed event bus (#8, ADR-013). Modules
//! are compile-time Rust code; there is no dynamic (DLL) loading.

pub mod config;
pub mod context;
pub mod contract;
#[cfg(feature = "demo-modules")]
pub mod demo;
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

/// Modules compiled into this build. A normal build has none yet; the feature
/// `demo-modules` adds the demo modules for manual checks (#12).
pub fn catalog() -> Vec<Box<dyn ModuleFactory>> {
    #[cfg(feature = "demo-modules")]
    return demo::factories();
    #[cfg(not(feature = "demo-modules"))]
    Vec::new()
}
