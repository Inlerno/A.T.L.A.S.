// Types of the module IPC (issue #11): a hand-written mirror of
// `src-tauri/src/dto`. A Rust test (`dto::modules::tests`) fails when a key or
// a tag serialized there is missing here.

/** Identifier of a module, e.g. `ai.lmstudio`. */
export type ModuleId = string;

export interface ModuleDescriptor {
  id: ModuleId;
  displayName: string;
  description: string;
  version: string;
  dependencies: ModuleId[];
  defaultEnabled: boolean;
}

export type FailurePhase =
  | "initialize"
  | "start"
  | "run"
  | "stop"
  | "healthCheck"
  | "settings";

export type BlockReason =
  | "dependencyFailed"
  | "dependencyDisabled"
  | "dependencyConfigUnavailable";

/** What is happening with the module now. `at` is Unix time in milliseconds. */
export type ModuleState =
  | { kind: "stopped" }
  | { kind: "initializing" }
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "stopping" }
  | { kind: "failed"; phase: FailurePhase; error: string; at: number }
  | { kind: "blocked"; by: ModuleId; reason: BlockReason }
  | { kind: "configUnavailable"; error: string; at: number };

/** Informational: a health result never starts, stops or restarts anything. */
export type Health =
  | { status: "healthy" }
  | { status: "degraded"; message: string }
  | { status: "unhealthy"; message: string };

export interface ModuleStatus {
  /** The user's decision; `null` — unknown (the configuration was not read). */
  enabled: boolean | null;
  state: ModuleState;
}

export interface Module {
  descriptor: ModuleDescriptor;
  /** The user's decision; `null` — unknown (the configuration was not read). */
  enabled: boolean | null;
  state: ModuleState;
  /** Last result for the running instance; `null` until it is checked. */
  health: Health | null;
  /** New settings apply only after a confirmed restart. */
  restartRequired: boolean;
}

export type ChangeAction = "enable" | "disable" | "recover" | "restart";

export type PlanStep =
  | { kind: "enable"; id: ModuleId }
  | { kind: "start"; id: ModuleId }
  | { kind: "stop"; id: ModuleId }
  | { kind: "disable"; id: ModuleId };

export interface Blocker {
  id: ModuleId;
  state: ModuleState;
}

export interface ChangePlan {
  action: ChangeAction;
  target: ModuleId;
  /** Shown to the user and sent back as `acknowledged`. */
  affected: ModuleId[];
  steps: PlanStep[];
  requiresConfirmation: boolean;
  /** Not empty when the plan cannot be applied. */
  blockers: Blocker[];
}

export interface FailedModule {
  id: ModuleId;
  error: string;
}

export interface ApplyReport {
  started: ModuleId[];
  stopped: ModuleId[];
  failed: FailedModule[];
  blocked: ModuleId[];
  interrupted: boolean;
}

/** Expected outcomes of applying a change; none of them is an error. */
export type ApplyResult =
  | { outcome: "applied"; report: ApplyReport }
  | { outcome: "confirmationRequired"; plan: ChangePlan }
  | { outcome: "impactChanged"; plan: ChangePlan }
  | { outcome: "notApplicable"; blockers: Blocker[] };

export interface ModuleSettings {
  id: ModuleId;
  /** A JSON object. */
  value: Record<string, unknown>;
  /** `false`: the stored value failed the checks at startup. */
  valid: boolean;
  restartRequired: boolean;
}

export type ModuleEvent =
  | { type: "statusChanged"; id: ModuleId; previous: ModuleStatus; current: ModuleStatus }
  | { type: "healthChanged"; id: ModuleId; health: Health }
  | { type: "settingsChanged"; id: ModuleId; restartRequired: boolean }
  | { type: "managerReady" };

/** Payload of `modules://event`. A gap in `seq` means missed events. */
export interface UiEvent {
  seq: number;
  atUnixMs: number;
  event: ModuleEvent;
}

/** Payload of `modules://resync`: load the modules again. */
export interface Resync {
  missed: number;
}

export type ErrorCode =
  | "invalid_id"
  | "unknown_module"
  | "action_not_applicable"
  | "invalid_settings"
  | "not_running"
  | "not_ready"
  | "shut_down"
  | "config_unavailable"
  | "store"
  | "internal";

/** A real error of a command. `message` is safe to show. */
export interface IpcError {
  code: ErrorCode;
  message: string;
}
