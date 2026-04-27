# Agentic Research

Use for HumanLayer-style isolated research without bloating the main context.

1. Read `AGENTS.md`, `docs/progress/STATUS.md`, and the relevant task prompt.
2. Read critical design docs yourself; do not outsource canonical context.
3. If subagents are explicitly authorized, dispatch focused sidecar tasks:
   locator for file discovery, analyzer for current behavior, pattern-finder for
   examples, and progress-memory locator for decisions/handoffs.
4. If subagents are not authorized, perform those roles locally and keep notes
   separated by role.
5. Synthesize findings into a short research note under
   `docs/progress/research/YYYY-MM-DD-short-title.md` when the result should
   survive the chat.
6. Cite `external/humanlayer-reference/.claude/` only as workflow reference;
   txKernel rules remain authoritative.
