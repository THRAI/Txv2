# Create Worktree

Use when parallel implementation needs isolation from the main workspace.

1. Choose a branch name with the `codex/` prefix unless the user asks otherwise.
2. Create a worktree outside the repository root.
3. Keep `docs/progress/` as the shared durable memory source.
4. Create the record with
   `cargo xtask progress new worktree --id YYYY-MM-DD-short-title --title "Short title" --branch codex/short-title --path /absolute/path/to/worktree --scope path/prefix`.
5. Add plan path, owner, write scope, verification, and notes as needed.
6. Validate with `cargo xtask progress validate`.
7. Remove the worktree only after the branch is merged or explicitly abandoned.
