# txKernel Agent Guide

txKernel is a Rust kernel architecture/spec workspace. The active docs define a factored kernel model: semantic subsystems own entities and transitions; substrate provides zone, index, epoch, mutation, bus, page, and reservation primitives; the reactor schedules tasks and waits; HAL is axHal-style static platform selection.

## Read Order

1. `docs/design/INDEX.md`
2. `docs/design/00_meta-framework/CONCEPTS_v4.md`
3. `docs/design/00_meta-framework/INVARIANTS_v4.md`
4. `docs/design/00_meta-framework/object_model_v2.md`
5. `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`

## Skills

Use `.agents/skills/` for task-specific guidance. Skills are generated or maintained from canonical docs and are the preferred operational entry points for edits, audits, and implementation planning.

Start with:

- `tx-agentic-development` when planning or running multi-agent work, especially
  HumanLayer-style locator/analyzer/pattern workflows.
- `tx-design-reference` when gathering the relevant active docs for a task.
- `tx-docs-cleanup` for broad cleanup walks.
- `tx-docs-skill-maintenance` when creating or refreshing skills.
- `tx-progress-memory` when recording or resuming decisions, plans, handoffs,
  research, or status.
- `tx-meta-alignment` when editing `docs/design/00_meta-framework/`.
- `tx-ebr-zone` when editing object model, EBR, zone, cap, weak, witness, or projection docs.
- `tx-hal-axhal` when editing HAL, boot, page substrate, traps, pmap, or platform docs.
- `tx-subsystem-manifest` when editing subsystem specs.
- `tx-implementation-readiness` when deciding if docs are ready to code from.

## Rules

- Active docs override archived and source-trace docs.
- Do not resurrect v11, OSTD HAL manager, runtime `HalManager`, or upper-layer raw `Zone<T, Policy>`.
- Upper subsystems expose role-shaped types: `Cap<T>`, `PayloadCap<T>`, `Weak<T>`, `IdentRef<'g, T>`, witnesses, identity slots, and projection rows.
- Active design docs carry grep-stable `txdoc:` tags. Use those tags in CI,
  review, and implementation-plan references.
- Before declaring any task complete, do a progress catch-up in
  `docs/progress/`: update `STATUS.md` and, when useful, close or update the
  relevant JSON plan/worktree/handoff or add a dated decision/research note.
  The catch-up must name what changed, verification run, next step, and any
  blocker. Validate changed JSON records with `cargo xtask progress validate`.
- After doc edits, check active Markdown links and stale vocabulary before declaring alignment.
- **Do not** add yourself as coauthor when creating commits.
