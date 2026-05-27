# txKernel Progress Memory

This folder records durable project memory: what we decided, what is in flight,
what another agent needs to resume, and which external findings matter.

It follows the same broad shape used by command-driven agent workspaces such as
HumanLayer's `.claude/commands` and `.claude/agents`: keep always-on guidance
small, put repeatable workflows in commands, and write resumable memory into
predictable files.

## Layout

- `STATUS.md` is the short current-state checkpoint.
- `decisions/` contains dated Markdown decision records.
- `plans/` contains JSON implementation or cleanup plans that may span sessions.
- `handoffs/` contains JSON resume packets for future agents.
- `worktrees/` contains JSON records for active or recently completed worktrees.
- `research/` contains Markdown external or codebase research notes.
- `templates/` contains record templates.

## Rules

- Use dated filenames. Operational records use
  `YYYY-MM-DD-short-kebab-title.json`; decisions and research use
  `YYYY-MM-DD-short-kebab-title.md`.
- Keep active architecture contracts in `docs/design/`; progress notes can point
  to the design docs but do not replace them.
- Keep `STATUS.md` brief and update it when a plan completes, a blocker appears,
  or a decision changes the next step.
- Every completed task requires a finish catch-up before the final response.
  Update `STATUS.md` and any relevant plan/worktree/handoff/decision/research
  record so future agents can see what changed, verification, next step, and
  blockers without replaying chat.
- Link to commands, files, and verification outputs whenever possible.
- Keep JSON records valid through `cargo xtask progress validate`. Prefer
  stable keys over prose blobs so agents can query records without loading full
  narrative context.

## Xtask Commands

```sh
cargo xtask progress validate
cargo xtask progress list all
cargo xtask progress list plans --json
cargo xtask progress new plan --id YYYY-MM-DD-short-title --title "Short title" --scope path/prefix
cargo xtask progress claim plan --id YYYY-MM-DD-short-title --owner agent-name --scope path/prefix
cargo xtask progress close plan --id YYYY-MM-DD-short-title --status complete
```

`progress validate` performs typed schema checks, date/id checks, reference
checks, and active write-scope overlap checks. `jq` remains useful for ad hoc
queries, but it is not the validation authority.

Thin wrapper scripts remain available for simple listings:

```sh
tools/progress/list-plans.sh
tools/progress/list-worktrees.sh
tools/progress/list-handoffs.sh
```

## Command Prompts

Reusable prompts live in `.agents/commands/`. They are not executable scripts;
they are small workflow contracts for recording decisions, updating status,
creating plans, writing handoffs, resuming work, preserving research, and
running the finish catch-up workflow.

HumanLayer's source workflow is kept as a reference-only sparse submodule at
`external/humanlayer-reference/.claude/`. Local txKernel skills and commands are
authoritative for txKernel work.
