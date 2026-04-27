# Create Plan

Use when work spans multiple files, sessions, or verification steps.

1. Create the skeleton with
   `cargo xtask progress new plan --id YYYY-MM-DD-short-title --title "Short title" --scope path/prefix`.
2. Fill in summary, scope, write sets, steps, verification, risks, and blockers.
3. Keep implementation details close to the files they affect.
4. Claim active write scopes with
   `cargo xtask progress claim plan --id YYYY-MM-DD-short-title --owner agent-name --scope path/prefix`.
5. Validate with `cargo xtask progress validate`.
6. Update `docs/progress/STATUS.md` if the plan becomes active.
