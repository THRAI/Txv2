# txKernel Agent Commands

These files are reusable prompt contracts, not shell scripts. They capture
repeatable workflows for durable project memory in `docs/progress/`.

Use them when a task needs a decision record, plan, handoff, research note,
status update, resume packet, or finish catch-up.

Operational records are JSON by default:

- plans: `docs/progress/plans/*.json`
- handoffs: `docs/progress/handoffs/*.json`
- worktrees: `docs/progress/worktrees/*.json`

Validate changed operational records with `cargo xtask progress validate`.
Use `cargo xtask progress list all --json` when a tool needs machine-readable
state.

Every completed task must run the finish catch-up workflow in
`finish-catchup.md` before the final response. The catch-up records what
changed, verification, next step, and blockers in `docs/progress/`.

HumanLayer workflow references are available under
`external/humanlayer-reference/.claude/`. txKernel command prompts adapt those
ideas to local paths, skills, and verification commands.
