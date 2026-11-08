# First development stage — Stage 0/1

## Goal
Create a clean, reproducible Windows desktop foundation. Do not implement all product features at once.

## Stage 0 — GitHub and Tauri scaffold
1. GitHub repository: `Inlerno/A.T.L.A.S.` (product name ATLAS). On Windows clone into a short ASCII path outside OneDrive with an explicit folder name, because the repository name ends with a dot: `git clone https://github.com/Inlerno/A.T.L.A.S..git C:\dev\atlas`.
2. Extract this package into repository root and commit.
3. Install Git for Windows, Node.js LTS, Rust stable and Tauri 2 Windows prerequisites (C++ Build Tools and WebView2 as required).
4. Generate the official Tauri 2 React + TypeScript + Vite starter. If generator requires an empty directory, generate in a temporary sibling directory and copy scaffold files without overwriting docs.
5. Run the official development command and verify the desktop window opens.
6. Record exact versions/commands in `docs/DEVELOPMENT_LOG.md`.

Do not guess generator flags or pin versions before checking current official Tauri instructions.

## Stage 1 — Minimal app skeleton
Implement only:
- minimal React app shell;
- minimal Tauri/Rust startup;
- one typed status/health command;
- typed DTO;
- structured logging;
- module contract + registry, no real feature modules yet;
- SQLite connection abstraction and one migration only after dependency setup is verified.

Out of scope: microphone, real LLM calls, Telegram, Windows automation, long-term memory, vector DB, arbitrary command execution.

## Acceptance criteria
- [ ] Fresh clone installs with documented commands.
- [ ] App launches on Windows 10/11.
- [ ] Frontend calls typed Rust status command and displays result.
- [ ] Tauri handlers remain thin; business logic in services.
- [ ] Module lifecycle contract has unit tests.
- [ ] Structured errors/logging do not leak secrets.
- [ ] No credentials or machine-specific paths committed.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] `cargo test` passes.
- [ ] Frontend typecheck/build pass using actual package scripts.
- [ ] Manual Windows checks and environment versions documented.

## Initial GitHub issues
Superseded by the agreed plan (tracking issue #13); one issue = one PR, strictly in order:

- #1 `chore: initialize Tauri 2 React TypeScript application` (PR #2)
- #5 `ci: add Windows CI checks`
- #6 `feat(core): add error model, logging and application state`
- #7 `feat(modules): define module status, dependency graph and change plans`
- #8 `feat(events): add typed central event bus`
- #9 `feat(modules): add lifecycle manager with failure isolation`
- #10 `feat(modules): add per-module settings, enabled state and startup (in-memory)`
- #11 `feat(ipc): expose module manager commands and UI event bridge`
- #12 `feat(ui): add module panel with confirmation and demo modules`

Notes:
- The former "typed app status command" is covered by `list_modules` (#11).
- The former "SQLite foundation and initial migration" will be a separate issue after #12; Stage 1 is not complete without it.

Keep each issue small enough for one review.
