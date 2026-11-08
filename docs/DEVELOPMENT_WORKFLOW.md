# Development workflow — Claude Chat + GitHub

## Connector limitation
Claude's GitHub integration syncs selected file names and contents from a repository branch. It is repository context, not a normal local filesystem/terminal/Git client. Do not assume Claude Chat can apply edits, run builds, create commits or open PRs.

Use Claude for architecture, code review, debugging, plans and complete file contents/diffs. Apply, test and publish changes locally.

## Loop
1. Create a narrow GitHub issue with acceptance criteria.
2. In Claude Project, sync relevant repository files.
3. Ask Claude to inspect and plan before drafting.
4. Request full file contents or precise unified diff with exact paths.
5. Apply locally in VS Code or another editor.
6. Run checks and paste exact output into Claude if they fail.
7. Review `git diff`; don't commit secrets or build output.
8. Commit and push locally.
9. Sync GitHub context before next task.

## Branches
- `main`: buildable baseline.
- `feature/<issue>-short-name`: one issue per branch.
- Merge after local checks and manual review.

## Git commands
```powershell
git status
git switch -c feature/1-tauri-foundation
git diff --check
git diff
git add <specific-files>
git commit -m "chore: initialize Tauri foundation"
git push -u origin feature/1-tauri-foundation
```

Human review is mandatory for dependencies/licenses, migrations, OS-control features, permissions/secrets, installer scripts and destructive operations.
