# Finish Catch-up

Use before declaring a task complete.

1. Read `docs/progress/STATUS.md` and any plan, handoff, worktree, decision, or
   research note touched by the task.
2. Update progress memory with the completed change:
   - update `docs/progress/STATUS.md` for current shape, blockers, and next
     step;
   - close or update relevant JSON plans/worktrees/handoffs when the task was
     tracked there;
   - add a dated decision or research note if architecture, workflow, or durable
     understanding changed.
3. The catch-up must name changed surface, verification commands/results, next
   action, and unresolved blockers.
4. Validate JSON progress records with `cargo xtask progress validate` whenever
   any JSON record changed.
5. Run the nearest relevant lint/check for changed Markdown or workflow docs
   before final response.
6. In the final response, mention the progress catch-up file(s) updated.
