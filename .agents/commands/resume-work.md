# Resume Work

Use when picking up txKernel work from durable memory.

1. Read `docs/progress/STATUS.md`.
2. List current JSON plans, handoffs, and worktrees:
   `cargo xtask progress list all`.
3. Read the newest relevant file under `docs/progress/handoffs/`,
   `docs/progress/plans/`, `docs/progress/worktrees/`,
   `docs/progress/decisions/`, or `docs/progress/research/`.
4. Check `git status --short`.
5. Continue from the recorded next action; do not restart the task unless the
   repo state contradicts the handoff.
6. Before declaring resumed work complete, run
   `.agents/commands/finish-catchup.md`.
7. Record a new JSON handoff if work remains incomplete.
