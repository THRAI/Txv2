---
date: 2026-04-29
topic: "Agent cooperation framework for txKernel"
status: complete
---

# Research: Agent Cooperation Framework for txKernel

## Question

What agent-cooperation framework can Tx adapt?

## Conclusion

txKernel should adapt HumanLayer's `.claude` command and specialist-agent
workflow, but treat it as a reference pattern only. The authoritative Tx version
is the local `.agents/skills/` plus `.agents/commands/` workflow backed by
`docs/progress/` durable memory.

This is a better fit than a generic autonomous-agent swarm because Tx needs
auditability, scoped authority, deterministic verification, and resumable
handoffs more than it needs open-ended agent negotiation.

Follow-up external scan: because Codex itself does not expose a native
multi-agent interface, Tx should distinguish two layers:

- Cooperation protocol: Tx-owned roles, write scopes, handoffs, verification,
  and progress memory.
- Execution harness: an external framework or workflow engine that can launch
  independent workers, pass artifacts, checkpoint state, and call Codex or other
  coding agents as tools.

For a mature execution harness, the best candidates to adapt are LangGraph,
Microsoft Agent Framework, Temporal, and A2A/MCP protocol boundaries. CrewAI,
Mastra, Pydantic AI, AG2, and mcp-agent are useful patterns or ecosystem
options, but they should not replace Tx's local progress-memory contract.

## Evidence

- `.agents/skills/tx-agentic-development/SKILL.md` already identifies the
  HumanLayer-style locator, analyzer, pattern-finder, research, worktree, plan,
  and handoff flow as the model to adapt.
- `docs/progress/README.md` already defines the Tx memory substrate: status,
  decisions, plans, handoffs, worktrees, research notes, and schema-validated
  operational JSON.
- `.agents/commands/agentic-research.md` and
  `.agents/commands/agentic-implement-plan.md` already localize the workflow:
  the main agent reads canonical docs, subagents are bounded sidecars when
  explicitly authorized, workers get disjoint write scopes, and verification is
  recorded before finish catch-up.

## Tx Adaptation

- Main agent owns the user task, canonical design-doc context, synthesis, and
  final decision.
- Locator role finds files and docs only.
- Analyzer role explains current behavior with references, without redesigning.
- Pattern-finder role catalogs existing examples and conventions.
- Progress-memory role finds relevant plans, decisions, handoffs, worktrees, and
  research notes.
- Worker role implements a bounded scope, respects other agents' edits, reports
  changed files, and records verification.
- Plans, worktrees, and handoffs stay as schema-checked JSON under
  `docs/progress/`; decisions and research stay as dated Markdown.

## Guardrails

- Active `docs/design/` docs and Tx skills override the HumanLayer reference.
- Codex sessions spawn subagents only when the user explicitly authorizes
  subagents, delegation, or parallel agent work; otherwise the same roles are
  performed locally as separated research modes.
- Parallel implementation requires disjoint write scopes.
- Finish catch-up records changed surface, verification, next step, and
  blockers before claiming completion.

## Mature Framework Scan

### LangGraph

LangGraph is the closest match for Tx's desired semantics: explicit graph
control, durable execution, human-in-the-loop interrupts, persistence, memory,
streaming, and stateful long-running workflows. Its docs position it as a
low-level orchestration framework rather than a high-level persona swarm.

Source: https://docs.langchain.com/oss/javascript/langgraph/overview
Source: https://docs.langchain.com/oss/python/langgraph/durable-execution

Tx fit: high. It maps naturally to plan phases, locator/analyzer/pattern
sidecars, verifier gates, and resumable handoffs. If Tx builds a real external
runner, LangGraph is the strongest first prototype.

### Microsoft Agent Framework

Microsoft Agent Framework reached 1.0 for .NET and Python in April 2026. The
official announcement calls it production-ready, with stable APIs and long-term
support. It includes graph workflows, sequential/concurrent/handoff/group-chat
and Magentic-One orchestration patterns, checkpointing, pause/resume, human
approvals, MCP support, and A2A support on the roadmap.

Source:
https://devblogs.microsoft.com/agent-framework/microsoft-agent-framework-version-1-0/

Tx fit: high if Tx wants a production-grade orchestrator and does not mind a
.NET/Python harness. It is heavier than LangGraph, but more enterprise-shaped
and explicitly absorbs AutoGen/Semantic Kernel lineage.

### Temporal

Temporal is not an agent framework; it is a mature durable-execution substrate.
Its workflow model captures state at every step and resumes after failures.
Temporal is useful when the cooperation problem becomes a reliability problem:
long-running workers, human review waits, retries, idempotent side effects, and
audit trails.

Source: https://temporal.io/

Tx fit: high as infrastructure underneath a Tx runner, especially if work spans
hours or days. It is likely too heavy for simple local research fan-out.

### A2A and MCP

A2A is an interoperability protocol for agent-to-agent communication. Its
official project describes JSON-RPC over HTTP, agent cards, streaming, push
notifications, and text/file/structured data exchange. MCP is the tool/context
protocol; it is useful for exposing Tx progress records, xtask commands, design
doc lookups, and verifier commands as controlled tools.

Source: https://github.com/a2aproject/A2A
Source: https://modelcontextprotocol.io/docs/sdk

Tx fit: high as boundaries, not as the scheduler. Use A2A if Tx agents become
separate services; use MCP for tool surfaces. Do not make either one the plan
authority.

### CrewAI

CrewAI has strong adoption and convenient crews/flows. Current docs cover
structured flows, state persistence, and human-feedback pauses.

Source: https://docs.crewai.com/en/concepts/flows
Source: https://github.com/orgs/crewAIInc/repositories

Tx fit: medium. Good for quick autonomous-agent experiments, but its default
"crew" framing is less aligned with Tx's need for deterministic write scopes,
canonical docs, and explicit verification gates.

### Mastra

Mastra is a TypeScript-first framework with agents, graph workflows,
human-in-the-loop suspension, storage-backed resume, MCP servers, evals, and
observability.

Source: https://github.com/mastra-ai/mastra

Tx fit: medium-high if the runner should be TypeScript/Node. It is attractive
for a local UI or web control plane, but less directly aligned with Tx's Rust
workspace than LangGraph or Microsoft Agent Framework.

### Pydantic AI

Pydantic AI documents several multi-agent patterns: delegation,
programmatic handoff, graph-based control flow, and deep agents with planning,
file operations, task delegation, and sandboxed code execution.

Source: https://pydantic.dev/docs/ai/guides/multi-agent-applications/

Tx fit: medium. Strong for typed Python agents and structured outputs, but more
library-like than a complete cooperation protocol.

### AG2 / AutoGen Lineage

AG2 and AutoGen-family systems remain important references for group chat,
speaker selection, sequential chat, nested chat, and human-in-the-loop
multi-agent conversation patterns.

Source: https://docs.ag2.ai/latest/docs/user-guide/advanced-concepts/orchestrations/
Source: https://github.com/microsoft/autogen

Tx fit: medium-low as a direct foundation. Useful pattern vocabulary, but Tx
should avoid open-ended group chat as the main mechanism for codebase work.

### mcp-agent

mcp-agent is worth watching because it combines Anthropic-style effective-agent
patterns with MCP-native tools and optional Temporal-backed durability.

Source: https://github.com/lastmile-ai/mcp-agent
Source: https://docs.mcp-agent.com/mcp-agent-sdk/advanced/durable-agents

Tx fit: medium-high for a prototype that treats Tx commands and docs as MCP
tools. It is less broadly mature than LangGraph/Microsoft/Temporal, but its
MCP-native shape fits the "Codex as one client, tools as shared surface" model.

## Updated Recommendation

Do not try to force Codex Desktop itself to become multi-agent. Adapt this stack
instead:

1. Tx Cooperation Protocol: local roles, progress JSON, worktree records,
   handoffs, verification, and finish catch-up.
2. LangGraph Runner: first external prototype for graph/state/human gates.
3. MCP Tool Surface: expose `cargo xtask progress`, design-doc lookup,
   verification commands, and scoped filesystem/worktree operations.
4. Optional A2A Boundary: only if workers become independent long-running
   services rather than local subprocesses.
5. Optional Temporal Backend: only when failures, retries, and day-scale waits
   justify durable infrastructure.

For immediate Tx work, the most useful external adaptation is LangGraph's
stateful graph/checkpoint model plus A2A/MCP-style boundaries. Microsoft Agent
Framework is the heavier production alternative; Temporal is the reliability
substrate when this grows beyond local orchestration.

## Codex Communication Model

Local Codex Desktop/CLI 0.125.0 provides three practical integration surfaces:

1. CLI subprocess: `codex exec` can run a non-interactive Codex session with a
   prompt from argv/stdin, a selected working directory, sandbox mode, optional
   JSONL events, and an output file for the last message. This is the simplest
   way for a LangGraph or Temporal runner to launch one isolated worker per
   worktree.
2. MCP server: `codex mcp-server` starts a stdio MCP server. Probing
   `tools/list` shows two tools:
   - `codex`: starts a Codex session with fields such as `prompt`, `cwd`,
     `sandbox`, `approval-policy`, `model`, `profile`, `developer-instructions`,
     and config overrides; returns `threadId` and `content`.
   - `codex-reply`: continues an existing Codex thread with `threadId` and a
     prompt; returns the same shape.
3. File protocol: regardless of runner, Tx should use `docs/progress/` records,
   worktree paths, verification logs, and patch diffs as the durable handoff
   surface. Chat content is a report, not the source of truth.

Recommended flow:

1. External runner reads a Tx plan JSON.
2. Runner allocates a worktree/write scope and records it under
   `docs/progress/worktrees/`.
3. Runner calls Codex through MCP `codex` or through `codex exec`.
4. Codex reads the assigned files, edits only the assigned scope, runs checks,
   and writes/updates progress artifacts.
5. Runner consumes Codex's final content plus filesystem artifacts, then either
   launches verifier/reviewer steps or asks the human for approval.

Use MCP when the orchestrator can act as an MCP client and wants persistent
thread IDs. Use `codex exec` when the runner wants simple stateless subprocess
workers and durable state lives entirely in Tx files.

## Concrete LangGraph Workflow

The graph should not be "published into" a Codex session as if Codex were the
orchestrator. Instead, a Tx runner owns the LangGraph graph outside Codex, and
Codex is one executor used by graph nodes.

Flow:

1. A human or Codex session creates or selects a Tx plan under
   `docs/progress/plans/`.
2. The external runner loads that plan and builds a LangGraph state machine:
   gather context, locate files, analyze current behavior, find patterns,
   prepare worker scopes, run workers, verify, review, and finish catch-up.
3. Each graph node receives a small structured state packet: plan id, phase id,
   role, allowed read/write scope, worktree path, required output files, and
   verification command.
4. For a Codex-backed node, the runner calls either MCP `codex` /
   `codex-reply` or `codex exec`.
5. Codex performs only that node's role and writes durable output back into the
   workspace: research notes, plan status, handoff JSON, verification results,
   changed files, or final summary.
6. The runner reads those artifacts plus Codex's final message, updates the
   LangGraph state, and decides the next edge: continue, retry, ask a human,
   launch verifier, launch reviewer, or stop blocked.
7. The current interactive Codex session can inspect the runner output and
   continue the human-facing conversation, but it is not the scheduler.

In short: Tx publishes a plan; the runner instantiates the graph; Codex workers
report through files plus MCP/CLI return values; the runner advances the graph.

## Command Infrastructure

LangGraph is sufficient as the orchestration core, but not as the whole
parallel-agent command system. Tx still needs a thin local runner that binds
LangGraph to Codex, worktrees, progress records, verification, and human gates.

Required layers:

1. Plan compiler
   - Reads `docs/progress/plans/*.json`.
   - Expands phases into graph nodes with dependencies, roles, scopes,
     verification commands, and expected artifacts.
   - Rejects overlapping write scopes before launch.

2. Worker adapter
   - Starts Codex via MCP `codex` / `codex-reply` or `codex exec`.
   - Injects role prompts, allowed read/write scope, worktree path, and output
     schema.
   - Captures thread id, stdout/JSONL events, final content, and exit status.

3. Worktree and lease manager
   - Creates or selects isolated worktrees.
   - Writes `docs/progress/worktrees/*.json`.
   - Claims path scopes and prevents active overlap.
   - Records owner, branch, plan id, phase id, and verification state.

4. Artifact protocol
   - Treats files as the source of truth: progress JSON, research notes,
     handoff JSON, verification logs, diffs, and test output.
   - Requires workers to report changed files, commands run, blockers, and next
     step.
   - Keeps chat output as a summary, not durable state.

5. Verifier/reviewer gates
   - Runs per-node checks and aggregate checks.
   - Launches read-only reviewer nodes when useful.
   - Blocks integration until `cargo xtask progress validate`, relevant tests,
     and diff checks pass.

6. Human control surface
   - Presents graph state, active workers, claimed scopes, blockers, and pending
     approvals.
   - Pauses before destructive operations, architecture changes, merges, or
     ambiguous scope expansion.

7. Concurrency and budget policy
   - Sets lane limits by role: high for read-only research, moderate for
     implementation, low for integration.
   - Applies timeouts, cancellation, retries, and model/tool budget limits.

8. Recovery and handoff
   - Can resume from LangGraph checkpoint plus `docs/progress/`.
   - Writes handoff JSON when a node or run stops incomplete.
   - Treats stale worktree leases as explicit recovery cases, not silent reuse.

MVP scope:

- A local Python runner using LangGraph.
- `codex exec` worker launch first; MCP thread persistence later.
- Existing `docs/progress` JSON as durable state.
- One worktree per implementation worker.
- Read-only locator/analyzer/pattern nodes.
- One verifier node.
- Human approval by terminal prompt or current Codex session summary.

This is enough to command parallel agents safely. A2A is unnecessary until
workers are independent services. Temporal is unnecessary until local
checkpointing and file-backed recovery are not enough.

## Parallelism Model

Useful parallelism is bounded by the plan dependency graph, not by the number of
available model sessions. The runner should compute parallel width from:

- Independent read-only research roles: locators, analyzers, pattern finders,
  and progress-memory scanners can fan out aggressively because they do not
  write shared code.
- Disjoint write scopes: implementation workers may run in parallel only when
  their assigned files, modules, generated artifacts, and progress records do
  not overlap.
- Shared verification bottlenecks: repository-wide checks, QEMU smoke runs,
  progress validation, final review, and merge integration usually serialize or
  run in small batches.
- Architectural dependency depth: if one phase defines a trait, type, schema,
  or invariant that later workers need, that phase sits on the critical path.
- Coordination overhead: too many workers increase prompt setup, review,
  merge-conflict, and failed-assumption costs.

For Tx, read-only discovery can often reach 4-8 useful parallel nodes. Safe
implementation is usually lower: 2-4 workers for clearly separated subsystems
or boards, sometimes 5-8 for broad mechanical doc/test updates with stable
scopes. Kernel-core changes that touch shared invariants, pmap, substrate init,
or object-model contracts often collapse to one primary implementer plus
parallel read-only analysis and verification.

The practical scheduling rule is:

`parallelism = min(independent DAG width, disjoint write scopes, verification
capacity, review capacity, model/tool budget)`

The runner should prefer high parallelism for read-only discovery, moderate
parallelism for implementation with owned worktrees, and low parallelism for
integration, invariant changes, and final verification.

## Verification

Initial pass read the relevant Tx skill, progress-memory guide, local command
prompts, and HumanLayer reference prompts. Follow-up pass searched mature
external frameworks and checked primary official docs or GitHub repositories
for LangGraph, Microsoft Agent Framework, Temporal, A2A, MCP, CrewAI, Mastra,
Pydantic AI, AG2, AutoGen, and mcp-agent. A later local probe checked
`codex --help`, `codex exec --help`, `codex mcp-server --help`,
`codex exec-server --help`, and MCP `tools/list` for the local Codex 0.125.0
install. No code behavior changed.

## Next Step

Prototype a small LangGraph-based local runner that consumes a
`docs/progress/plans/*.json` record, launches Codex through MCP or `codex exec`
against an explicitly claimed worktree, runs read-only locator/analyzer/pattern
steps, emits artifacts back into `docs/progress/research/`, and leaves
implementation to scoped worker phases plus verifier gates.

## Blockers

None.
