# tx-agent-runner

Local MVP runner for coordinating bounded Codex workers against txKernel
`docs/progress/worktrees/*.json` lane records.

## Usage

```sh
uv run --project tools/agent-runner tx-agent-runner inspect --worktrees active
uv run --project tools/agent-runner tx-agent-runner run --worktrees active --mode status --max-workers 2 --dry-run
uv run --project tools/agent-runner tx-agent-runner run --worktree-id 2026-04-29-reactor-smoke --mode execute --task "Implement the lane plan" --max-workers 1
```

The default `status` mode launches read-only workers. `execute` mode requires an
explicit `--task` and gives the worker `workspace-write` sandboxing inside its
assigned worktree.

Run artifacts are written under `target/tx-agent-runs/<run-id>/`:

- `state.json`
- `<worktree-id>.prompt.md`
- `<worktree-id>.events.jsonl`
- `<worktree-id>.final.md`
- `summary.json`

The MVP validates active worktree paths, branch/path matches from
`git worktree list --porcelain`, and active write-scope overlap before dispatch.
It does not create worktrees, merge, commit, push, or provide durable MCP/A2A
sessions.
