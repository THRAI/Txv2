# Create Handoff

Use before stopping in the middle of nontrivial work, or when the finish
catch-up needs to leave explicit resume context.

1. Create the skeleton with
   `cargo xtask progress new handoff --id YYYY-MM-DD-short-title --title "Short title" --from agent-name`.
2. Record current state, changed files, verification, next actions, and caveats.
3. Include exact commands already run and their result.
4. Validate with `cargo xtask progress validate`.
5. Update `docs/progress/STATUS.md` if the handoff captures the latest state.
