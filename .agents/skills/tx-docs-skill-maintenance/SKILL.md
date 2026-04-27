---
name: tx-docs-skill-maintenance
description: Use when creating, regenerating, pruning, or auditing txKernel agent skills from canonical docs.
---

# tx-docs-skill-maintenance

Skills are task harnesses, not new architecture docs. Maintain them from canonical docs and keep them short enough to be useful when loaded.

## Read First

- `AGENTS.md`
- `.agents/skills/README.md`
- `.agents/skills/skill_manifest.yaml`
- `docs/design/INDEX.md`
- The canonical docs listed for the skill in `skill_manifest.yaml`

## Maintenance Rules

- A skill may summarize canonical docs, but must not invent doctrine.
- If a skill conflicts with a canonical doc, fix the skill unless the user explicitly asks to change the architecture.
- Prefer progressive disclosure: put only task-critical instructions in `SKILL.md`; put longer templates in `references/`.
- Avoid directory tours and long background. Agents can discover file trees.
- Every skill should answer:
  - When should I use this?
  - What canonical docs do I read first?
  - What must I preserve?
  - What checks prove I did not break it?

## HumanLayer-Inspired Shape

- Keep always-on guidance small.
- Use skills for reusable task knowledge.
- Treat skills like dependencies: inspect them, keep them scoped, and avoid bloated instruction payloads.
- If an agent made a repeatable mistake, add the smallest deterministic skill/check that prevents recurrence.

## Done Means

- `skill_manifest.yaml` lists the skill and canonical source docs.
- The skill is under focused task scope.
- The skill points back to canonical docs instead of duplicating large sections.
- Any new forbidden terms or required checks are also represented in `tx-docs-cleanup` or the appropriate domain skill.
