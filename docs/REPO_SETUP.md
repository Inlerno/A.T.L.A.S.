# GitHub repository setup

1. Repository: `Inlerno/A.T.L.A.S.` (product name ATLAS). Clone on Windows with an explicit folder name: `git clone https://github.com/Inlerno/A.T.L.A.S..git C:\dev\atlas` (a trailing dot in a folder name is stripped by Windows).
2. Do not auto-generate README or .gitignore; this package contains them. Choose a license deliberately after dependency/license review.
3. Extract package contents to repository root; ensure `CLAUDE.md` is at root.
4. Commit the documentation baseline.
5. Give Claude access to this repository only (session repository attachment). Claude works per ADR-010: branches, commits, issues and PRs; the user merges.
6. Optional: a Claude Project for plans and audits; the repository remains the source of truth.
7. Keep `CLAUDE.md` as the single source of project rules.

Suggested labels: `stage:foundation`, `type:feature`, `type:bug`, `type:chore`, `type:docs`, `priority:high`, `priority:medium`, `priority:low`, `needs-decision`.

Do not start voice, Telegram, memory or PC automation before the desktop foundation and typed IPC are verified on the target Windows machine.
