//! Tauri commands: the thin IPC layer (issue #11).
//!
//! A command only parses its arguments, calls a service of the core and maps
//! the result to a DTO. Business logic — plans, confirmation checks, settings
//! checks — lives in the module manager and is not repeated here.

pub mod modules;
