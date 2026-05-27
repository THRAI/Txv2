# Agentic Implement Plan

Use when executing a recorded plan.

1. Read the plan completely.
2. Read every file named by the current phase before editing.
3. If subagents are explicitly authorized, split only independent work with
   disjoint write scopes.
4. Tell workers they are not alone in the codebase, must not revert others'
   edits, and must report changed files plus verification.
5. Run the plan's verification command, usually `cargo xtask check` or a
   narrower named check.
6. Update JSON plan step statuses and verification entries.
7. Validate changed progress records with `cargo xtask progress validate`.
8. Run `.agents/commands/finish-catchup.md`: update `STATUS.md`, close or
   update the plan/worktree records, record verification, and name the next
   step/blockers.
9. Create a JSON handoff if work remains incomplete.
