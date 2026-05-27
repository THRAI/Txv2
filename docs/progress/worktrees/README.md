# Worktrees

Store one JSON record per active or recently completed worktree:
`YYYY-MM-DD-short-title.json`.

Use `.agents/commands/create-worktree.md` or create records directly with
`cargo xtask progress new worktree`.

Useful listing command:

```sh
cargo xtask progress list worktrees
cargo xtask progress list worktrees --json
```
