# First development stage — Stage 0/1

## Goal
Create a clean, reproducible Windows desktop foundation. Do not implement all product features at once.

## Stage 0 — GitHub and Tauri scaffold
1. Create GitHub repository `local-ai-assistant` (private initially).
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
- [ ] `cargo test` passes.
- [ ] Frontend typecheck/build pass using actual package scripts.
- [ ] Manual Windows checks and environment versions documented.

## Initial GitHub issues
1. `chore: initialize Tauri 2 React TypeScript application`
2. `feat(core): add application state and lifecycle`
3. `feat(modules): define module contract and registry`
4. `feat(ipc): add typed app status command`
5. `feat(storage): add SQLite foundation and initial migration`
6. `chore: add CI checks and development documentation`

Keep each issue small enough for one review.
