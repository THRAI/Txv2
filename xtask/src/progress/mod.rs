use serde::{Deserialize, Serialize};

use crate::Result;

mod commands;
mod validate;

pub(crate) use commands::progress;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressKind {
    Plan,
    Handoff,
    Worktree,
}

impl ProgressKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "plan" | "plans" => Ok(Self::Plan),
            "handoff" | "handoffs" => Ok(Self::Handoff),
            "worktree" | "worktrees" => Ok(Self::Worktree),
            other => Err(format!(
                "unknown progress kind '{other}', expected plan, handoff, or worktree"
            )),
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Plan => "docs/progress/plans",
            Self::Handoff => "docs/progress/handoffs",
            Self::Worktree => "docs/progress/worktrees",
        }
    }

    fn schema(self) -> &'static str {
        match self {
            Self::Plan => "tx.progress.plan.v1",
            Self::Handoff => "tx.progress.handoff.v1",
            Self::Worktree => "tx.progress.worktree.v1",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Handoff => "handoff",
            Self::Worktree => "worktree",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PlanStatus {
    Proposed,
    Active,
    Blocked,
    Complete,
    Canceled,
}

impl PlanStatus {
    fn active_for_claims(self) -> bool {
        matches!(self, Self::Proposed | Self::Active | Self::Blocked)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum StepStatus {
    Pending,
    InProgress,
    Blocked,
    Complete,
    Canceled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VerificationStatus {
    NotRun,
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum HandoffStatus {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum WorktreeStatus {
    Active,
    Merged,
    Abandoned,
    Closed,
}

impl WorktreeStatus {
    fn active_for_claims(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanV1 {
    schema: String,
    id: String,
    title: String,
    status: PlanStatus,
    created: String,
    updated: String,
    owner: String,
    summary: String,
    scope: ScopeV1,
    context: ContextV1,
    steps: Vec<PlanStepV1>,
    verification: Vec<VerificationV1>,
    risks: Vec<String>,
    blockers: Vec<String>,
    handoff: HandoffRefV1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScopeV1 {
    #[serde(rename = "in")]
    in_scope: Vec<String>,
    out: Vec<String>,
    write_sets: Vec<WriteSetV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteSetV1 {
    owner: String,
    paths: Vec<String>,
    notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextV1 {
    design_docs: Vec<String>,
    progress_refs: Vec<String>,
    external_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanStepV1 {
    id: String,
    title: String,
    status: StepStatus,
    depends_on: Vec<String>,
    paths: Vec<String>,
    verification: Vec<String>,
    notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerificationV1 {
    command: String,
    status: VerificationStatus,
    notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HandoffRefV1 {
    needed: bool,
    path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HandoffV1 {
    schema: String,
    id: String,
    title: String,
    created: String,
    from: String,
    status: HandoffStatus,
    plan: Option<String>,
    current_state: Vec<String>,
    changed_files: Vec<ChangedFileV1>,
    verification: Vec<VerificationV1>,
    next_actions: Vec<NextActionV1>,
    caveats: Vec<String>,
    links: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChangedFileV1 {
    path: String,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NextActionV1 {
    id: String,
    title: String,
    paths: Vec<String>,
    notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorktreeV1 {
    schema: String,
    id: String,
    created: String,
    updated: String,
    status: WorktreeStatus,
    branch: String,
    path: String,
    base: String,
    plan: Option<String>,
    owner: String,
    write_scope: Vec<String>,
    verification: Vec<String>,
    notes: String,
}

#[derive(Clone, Debug)]
struct ProgressRecordSummary {
    kind: ProgressKind,
    id: String,
    status: String,
    owner: String,
    title: String,
    path: String,
}

#[derive(Clone, Debug)]
struct ActiveClaim {
    kind: ProgressKind,
    id: String,
    owner: String,
    path: String,
    scopes: Vec<String>,
}
