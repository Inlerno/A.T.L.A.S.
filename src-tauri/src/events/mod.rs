//! Central typed event bus (issue #8, ADR-013).
//!
//! The only mechanism for notifications between the core, modules and the UI
//! (through [`ui_bridge`], #11): no direct callbacks, observers, separate channels or ways
//! around the bus (decision 24 in #13). The bus only delivers events: it never
//! changes module statuses and never starts enable, disable, recover, restart
//! or any automatic recovery (decision 25). Delivery guarantees are described
//! in [`bus`].

pub mod bus;
pub mod types;
pub mod ui_bridge;
