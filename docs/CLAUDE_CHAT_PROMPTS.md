# Ready prompts for Claude Chat

## First repository review
Read `CLAUDE.md`, `README.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `docs/ROADMAP.md` and `docs/FIRST_STAGE.md` from the connected GitHub repository. Do not write code yet. Summarize confirmed requirements, architecture boundaries, contradictions, Windows risks and the smallest first issue. Claude has direct write access (branches, commits, PRs) but must not change anything until the plan is approved.

## Plan one issue
For issue #[NUMBER], inspect relevant current files. Return understanding, acceptance criteria, plan, files to change, dependencies/risks and exact PowerShell verification commands. Do not code until plan is approved. Do not expand scope.

## Draft implementation
Implement the approved plan for issue #[NUMBER]. Return complete contents for every new/changed file or precise unified diff, with repository-relative paths. Include tests and commands. Do not claim tests passed.

## Review local diff
I will paste `git diff` and command output. Review correctness, Rust ownership/concurrency, frontend typing/IPC, security/secrets, Windows issues and scope creep. Classify Critical/High/Medium/Low and provide precise fixes.

## Debug
I will provide exact command, full error output and relevant files. Identify the first causal error. Give a minimal patch and verification command. Ask for missing diagnostics rather than guessing.

## Close issue
Summarize changed files, behavior, tests actually run and observed results, limitations, manual checks remaining and next issue.
