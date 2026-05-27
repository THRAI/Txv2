# Agentic Plan

Use for implementation plans that may involve isolated research or multiple
workers.

1. Read `.agents/skills/tx-agentic-development/SKILL.md`.
2. Gather current-state facts through locator/analyzer/pattern roles.
3. Create a plan skeleton with `cargo xtask progress new plan`.
4. Fill `docs/progress/plans/YYYY-MM-DD-short-title.json`.
5. Include ownership boundaries, write scopes, verification commands, and
   rollback or handoff notes.
6. Validate with `cargo xtask progress validate`.
7. Update `docs/progress/STATUS.md` if the plan becomes active.
