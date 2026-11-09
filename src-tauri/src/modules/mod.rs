//! Module Manager building blocks.
//!
//! Issue #7 (MM-2): pure data and logic — module identifiers and statuses,
//! the dependency graph and two-phase change plans (ADR-011, ADR-012).
//! The lifecycle manager (#9) applies plans; status changes are published only
//! through the central typed event bus (#8, ADR-013). Modules are compile-time
//! Rust code; there is no dynamic (DLL) loading.

pub mod contract;
pub mod graph;
pub mod plan;
