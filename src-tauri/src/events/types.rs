//! Event types of the central bus (issue #8, ADR-013).
//!
//! Strictly typed: every event is a variant of [`AppEvent`], grouped by domain,
//! so subscribers match exhaustively and never receive an untyped payload.
//! A variant is added together with the code that publishes it (decision 5 in
//! #13): `HealthChanged` with the lifecycle manager (#9), `SettingsChanged`
//! with settings (#10), the demo event with #12. Events are notifications
//! only and never carry secrets.

use crate::modules::contract::{ModuleId, ModuleStatus};

/// An event as delivered to subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    /// Publication number assigned by the bus: 1, 2, 3, … in the order in
    /// which events were accepted. Every subscriber receives events in
    /// strictly increasing `seq` order.
    pub seq: u64,
    /// Wall-clock publication time, Unix milliseconds. Informational only:
    /// the order of events is defined by `seq`.
    pub at_unix_ms: u64,
    /// Publisher of the event, stamped by the bus; a sender cannot choose it.
    pub source: EventSource,
    pub event: AppEvent,
}

/// Publisher of an event.
///
/// `Module(ModuleId)` is added in #12 together with publication by modules;
/// the core will stamp it, so a module cannot publish on behalf of another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSource {
    /// The application core, e.g. the lifecycle manager (#9).
    Core,
}

/// Every application event, grouped by domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Module(ModuleEvent),
}

/// Events of the module domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleEvent {
    /// The status of a module changed. Published only by the lifecycle
    /// manager (#9), the only component allowed to change statuses
    /// (decision 23 in #13).
    StatusChanged {
        id: ModuleId,
        previous: ModuleStatus,
        current: ModuleStatus,
    },
}
