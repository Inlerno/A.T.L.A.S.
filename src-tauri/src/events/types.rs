//! Event types of the central bus (issue #8, ADR-013).
//!
//! Strictly typed: every event is a variant of [`AppEvent`], grouped by domain,
//! so subscribers match exhaustively and never receive an untyped payload.
//! A variant is added together with the code that publishes it (decision 5 in
//! #13): `HealthChanged` with the lifecycle manager (#9), `SettingsChanged`
//! and `ManagerReady` with settings (#10), the demo event with #12. Events are notifications
//! only and never carry secrets.

use crate::modules::contract::{Health, ModuleId, ModuleStatus};

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
    /// The result of an on-demand health check differs from the previous one
    /// for the same running instance. Informational: it never changes the
    /// status (#9).
    HealthChanged { id: ModuleId, health: Health },
    /// The settings of a module were changed by the user and saved (#10).
    /// Carries no values: settings may hold user data. `restart_required` —
    /// the running instance was created with other settings and gets the new
    /// ones only after a confirmed `restart`; nothing restarts on its own.
    SettingsChanged {
        id: ModuleId,
        restart_required: bool,
    },
    /// The manager has loaded the configuration and finished the startup of
    /// enabled modules, so its snapshot is reliable (#10, ADR-015). Published
    /// once. It does not promise that the manager still accepts commands: the
    /// application may already be exiting.
    ManagerReady,
}
