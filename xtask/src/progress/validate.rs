use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::*;
use crate::util::{collect_files, relative};
use crate::Result;

pub(super) fn progress_validate(root: &Path) -> Result<()> {
    let mut files = Vec::new();
    for path in collect_files(root, &["json"]).map_err(|err| err.to_string())? {
        let normalized = relative(root, &path).replace('\\', "/");
        if normalized.starts_with("docs/progress/templates/")
            || normalized.starts_with("docs/progress/plans/")
            || normalized.starts_with("docs/progress/handoffs/")
            || normalized.starts_with("docs/progress/worktrees/")
        {
            files.push(path);
        }
    }
    files.sort();

    for file in &files {
        let is_template = relative(root, file)
            .replace('\\', "/")
            .starts_with("docs/progress/templates/");
        validate_progress_file(root, file, is_template)?;
    }
    validate_claim_overlaps(root)?;

    println!("progress records: ok ({} file(s))", files.len());
    Ok(())
}

pub(super) fn validate_progress_file(
    root: &Path,
    file: &Path,
    template: bool,
) -> Result<ProgressKind> {
    let text =
        fs::read_to_string(file).map_err(|err| format!("{}: {err}", relative(root, file)))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|err| format!("{}: invalid JSON: {err}", relative(root, file)))?;
    let schema = value
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing string field `schema`", relative(root, file)))?;

    let kind = match schema {
        "tx.progress.plan.v1" => {
            let record = serde_json::from_value::<PlanV1>(value)
                .map_err(|err| format!("{}: invalid plan schema: {err}", relative(root, file)))?;
            validate_plan(root, file, &record, template)?;
            ProgressKind::Plan
        }
        "tx.progress.handoff.v1" => {
            let record = serde_json::from_value::<HandoffV1>(value).map_err(|err| {
                format!("{}: invalid handoff schema: {err}", relative(root, file))
            })?;
            validate_handoff(root, file, &record, template)?;
            ProgressKind::Handoff
        }
        "tx.progress.worktree.v1" => {
            let record = serde_json::from_value::<WorktreeV1>(value).map_err(|err| {
                format!("{}: invalid worktree schema: {err}", relative(root, file))
            })?;
            validate_worktree(root, file, &record, template)?;
            ProgressKind::Worktree
        }
        other => {
            return Err(format!(
                "{}: unsupported progress schema `{other}`",
                relative(root, file)
            ));
        }
    };
    Ok(kind)
}

pub(super) fn validate_plan(root: &Path, file: &Path, plan: &PlanV1, template: bool) -> Result<()> {
    validate_schema(root, file, &plan.schema, ProgressKind::Plan)?;
    validate_common_record(
        root,
        file,
        &plan.id,
        &plan.title,
        &plan.created,
        Some(&plan.updated),
        template,
    )?;
    require_nonempty(root, file, "owner", &plan.owner, template)?;
    require_nonempty(root, file, "summary", &plan.summary, template)?;
    validate_scope(root, file, &plan.scope, template)?;
    validate_context(root, file, &plan.context, template)?;
    validate_steps(root, file, &plan.steps)?;
    validate_verification(root, file, &plan.verification)?;
    if let Some(path) = &plan.handoff.path {
        validate_progress_reference(root, file, "handoff.path", path, template)?;
    }
    Ok(())
}

pub(super) fn validate_handoff(
    root: &Path,
    file: &Path,
    handoff: &HandoffV1,
    template: bool,
) -> Result<()> {
    validate_schema(root, file, &handoff.schema, ProgressKind::Handoff)?;
    validate_common_record(
        root,
        file,
        &handoff.id,
        &handoff.title,
        &handoff.created,
        None,
        template,
    )?;
    require_nonempty(root, file, "from", &handoff.from, template)?;
    if let Some(plan) = &handoff.plan {
        validate_progress_reference(root, file, "plan", plan, template)?;
    }
    for changed in &handoff.changed_files {
        validate_repo_relative_path(&changed.path)
            .map_err(|err| format!("{}: changed_files.path: {err}", relative(root, file)))?;
        require_nonempty(
            root,
            file,
            "changed_files.reason",
            &changed.reason,
            template,
        )?;
    }
    validate_verification(root, file, &handoff.verification)?;
    validate_next_actions(root, file, &handoff.next_actions)?;
    Ok(())
}

pub(super) fn validate_worktree(
    root: &Path,
    file: &Path,
    worktree: &WorktreeV1,
    template: bool,
) -> Result<()> {
    validate_schema(root, file, &worktree.schema, ProgressKind::Worktree)?;
    validate_common_record(
        root,
        file,
        &worktree.id,
        "worktree",
        &worktree.created,
        Some(&worktree.updated),
        template,
    )?;
    require_nonempty(root, file, "owner", &worktree.owner, template)?;
    require_nonempty(root, file, "base", &worktree.base, template)?;
    if !template && !worktree.branch.starts_with("codex/") {
        return Err(format!(
            "{}: worktree branch `{}` must start with `codex/`",
            relative(root, file),
            worktree.branch
        ));
    }
    if !template && !Path::new(&worktree.path).is_absolute() {
        return Err(format!(
            "{}: worktree path must be absolute",
            relative(root, file)
        ));
    }
    if let Some(plan) = &worktree.plan {
        validate_progress_reference(root, file, "plan", plan, template)?;
    }
    for scope in &worktree.write_scope {
        if !template {
            validate_repo_relative_path(scope)
                .map_err(|err| format!("{}: write_scope: {err}", relative(root, file)))?;
        }
    }
    for command in &worktree.verification {
        require_nonempty(root, file, "verification command", command, template)?;
    }
    Ok(())
}

fn validate_schema(root: &Path, file: &Path, schema: &str, kind: ProgressKind) -> Result<()> {
    if schema == kind.schema() {
        Ok(())
    } else {
        Err(format!(
            "{}: expected schema `{}`, got `{schema}`",
            relative(root, file),
            kind.schema()
        ))
    }
}

fn validate_common_record(
    root: &Path,
    file: &Path,
    id: &str,
    title: &str,
    created: &str,
    updated: Option<&str>,
    template: bool,
) -> Result<()> {
    require_nonempty(root, file, "id", id, template)?;
    require_nonempty(root, file, "title", title, template)?;
    if template {
        return Ok(());
    }
    validate_id(id).map_err(|err| format!("{}: id: {err}", relative(root, file)))?;
    let expected = format!("{id}.json");
    let actual = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if actual != expected {
        return Err(format!(
            "{}: filename must be `{expected}` for id `{id}`",
            relative(root, file)
        ));
    }
    validate_date(created).map_err(|err| format!("{}: created: {err}", relative(root, file)))?;
    if created != id_date(id)? {
        return Err(format!(
            "{}: created date `{created}` must match id date `{}`",
            relative(root, file),
            id_date(id)?
        ));
    }
    if let Some(updated) = updated {
        validate_date(updated)
            .map_err(|err| format!("{}: updated: {err}", relative(root, file)))?;
    }
    Ok(())
}

fn validate_scope(root: &Path, file: &Path, scope: &ScopeV1, template: bool) -> Result<()> {
    for path in scope.in_scope.iter().chain(scope.out.iter()) {
        if !template {
            validate_repo_relative_path(path)
                .map_err(|err| format!("{}: scope path: {err}", relative(root, file)))?;
        }
    }
    for write_set in &scope.write_sets {
        require_nonempty(root, file, "write_sets.owner", &write_set.owner, template)?;
        if write_set.paths.is_empty() {
            return Err(format!(
                "{}: write_sets.paths must not be empty",
                relative(root, file)
            ));
        }
        for path in &write_set.paths {
            if !template {
                validate_repo_relative_path(path)
                    .map_err(|err| format!("{}: write_sets.path: {err}", relative(root, file)))?;
            }
        }
    }
    Ok(())
}

fn validate_context(root: &Path, file: &Path, context: &ContextV1, template: bool) -> Result<()> {
    for path in &context.design_docs {
        validate_repo_relative_path(path)
            .map_err(|err| format!("{}: context.design_docs: {err}", relative(root, file)))?;
        if !template && !root.join(path).exists() {
            return Err(format!(
                "{}: design doc reference `{path}` does not exist",
                relative(root, file)
            ));
        }
    }
    for path in &context.progress_refs {
        validate_progress_reference(root, file, "context.progress_refs", path, template)?;
    }
    Ok(())
}

fn validate_steps(root: &Path, file: &Path, steps: &[PlanStepV1]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for step in steps {
        require_nonempty(root, file, "steps.id", &step.id, false)?;
        require_nonempty(root, file, "steps.title", &step.title, false)?;
        if !ids.insert(step.id.clone()) {
            return Err(format!(
                "{}: duplicate step id `{}`",
                relative(root, file),
                step.id
            ));
        }
        for path in &step.paths {
            validate_repo_relative_path(path)
                .map_err(|err| format!("{}: steps.paths: {err}", relative(root, file)))?;
        }
        for command in &step.verification {
            require_nonempty(root, file, "steps.verification", command, false)?;
        }
    }
    for step in steps {
        for dep in &step.depends_on {
            if !ids.contains(dep) {
                return Err(format!(
                    "{}: step `{}` depends on unknown step `{dep}`",
                    relative(root, file),
                    step.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_verification(root: &Path, file: &Path, verification: &[VerificationV1]) -> Result<()> {
    for entry in verification {
        require_nonempty(root, file, "verification.command", &entry.command, false)?;
    }
    Ok(())
}

fn validate_next_actions(root: &Path, file: &Path, actions: &[NextActionV1]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for action in actions {
        require_nonempty(root, file, "next_actions.id", &action.id, false)?;
        require_nonempty(root, file, "next_actions.title", &action.title, false)?;
        if !ids.insert(action.id.clone()) {
            return Err(format!(
                "{}: duplicate next action id `{}`",
                relative(root, file),
                action.id
            ));
        }
        for path in &action.paths {
            validate_repo_relative_path(path)
                .map_err(|err| format!("{}: next_actions.paths: {err}", relative(root, file)))?;
        }
    }
    Ok(())
}

pub(super) fn progress_record_files(root: &Path, kind: ProgressKind) -> Result<Vec<PathBuf>> {
    let dir = root.join(kind.dir());
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|err| format!("{}: {err}", relative(root, &dir)))? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "json")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn progress_record_path(root: &Path, kind: ProgressKind, id: &str) -> PathBuf {
    root.join(kind.dir()).join(format!("{id}.json"))
}

pub(super) fn progress_summary(
    root: &Path,
    kind: ProgressKind,
    file: &Path,
) -> Result<ProgressRecordSummary> {
    let actual = validate_progress_file(root, file, false)?;
    if actual != kind {
        return Err(format!(
            "{}: schema belongs to {}, but file is listed under {}",
            relative(root, file),
            actual.label(),
            kind.label()
        ));
    }

    match kind {
        ProgressKind::Plan => {
            let record: PlanV1 = read_progress_json(file)?;
            Ok(ProgressRecordSummary {
                kind,
                id: record.id,
                status: json_enum_string(&record.status),
                owner: record.owner,
                title: record.title,
                path: relative(root, file),
            })
        }
        ProgressKind::Handoff => {
            let record: HandoffV1 = read_progress_json(file)?;
            Ok(ProgressRecordSummary {
                kind,
                id: record.id,
                status: json_enum_string(&record.status),
                owner: record.from,
                title: record.title,
                path: relative(root, file),
            })
        }
        ProgressKind::Worktree => {
            let record: WorktreeV1 = read_progress_json(file)?;
            Ok(ProgressRecordSummary {
                kind,
                id: record.id,
                status: json_enum_string(&record.status),
                owner: record.owner,
                title: record.branch,
                path: relative(root, file),
            })
        }
    }
}

pub(super) fn option_values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2)
        .filter_map(|pair| {
            if pair[0] == name {
                Some(pair[1].clone())
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn write_new_json(root: &Path, path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", relative(root, parent)))?;
    }
    fs::write(path, format!("{text}\n")).map_err(|err| format!("{}: {err}", relative(root, path)))
}

pub(super) fn read_progress_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

pub(super) fn write_json_value<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, format!("{text}\n")).map_err(|err| format!("{}: {err}", path.display()))
}

pub(super) fn json_enum_string<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

pub(super) fn reject_claim_overlap(
    root: &Path,
    kind: ProgressKind,
    id: &str,
    owner: &str,
    scopes: &[String],
) -> Result<()> {
    for scope in scopes {
        validate_repo_relative_path(scope)?;
    }
    for claim in active_claims(root)? {
        if claim.kind == kind && claim.id == id {
            continue;
        }
        for wanted in scopes {
            for existing in &claim.scopes {
                if scopes_overlap(wanted, existing) {
                    return Err(format!(
                        "{} `{id}` claim by `{owner}` for `{}` overlaps active {} `{}` claim by `{}` at {} for `{}`",
                        kind.label(),
                        wanted,
                        claim.kind.label(),
                        claim.id,
                        claim.owner,
                        claim.path,
                        existing
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_claim_overlaps(root: &Path) -> Result<()> {
    let claims = active_claims(root)?;
    for (idx, left) in claims.iter().enumerate() {
        for right in claims.iter().skip(idx + 1) {
            for left_scope in &left.scopes {
                for right_scope in &right.scopes {
                    if scopes_overlap(left_scope, right_scope) {
                        return Err(format!(
                            "active claim overlap: {} `{}` ({}) scope `{}` conflicts with {} `{}` ({}) scope `{}`",
                            left.kind.label(),
                            left.id,
                            left.path,
                            left_scope,
                            right.kind.label(),
                            right.id,
                            right.path,
                            right_scope
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn active_claims(root: &Path) -> Result<Vec<ActiveClaim>> {
    let mut claims = Vec::new();

    for file in progress_record_files(root, ProgressKind::Plan)? {
        let plan: PlanV1 = read_progress_json(&file)?;
        if !plan.status.active_for_claims() {
            continue;
        }
        for write_set in &plan.scope.write_sets {
            if write_set.paths.is_empty() {
                continue;
            }
            claims.push(ActiveClaim {
                kind: ProgressKind::Plan,
                id: plan.id.clone(),
                owner: write_set.owner.clone(),
                path: relative(root, &file),
                scopes: write_set.paths.clone(),
            });
        }
    }

    for file in progress_record_files(root, ProgressKind::Worktree)? {
        let worktree: WorktreeV1 = read_progress_json(&file)?;
        if !worktree.status.active_for_claims() || worktree.write_scope.is_empty() {
            continue;
        }
        claims.push(ActiveClaim {
            kind: ProgressKind::Worktree,
            id: worktree.id,
            owner: worktree.owner,
            path: relative(root, &file),
            scopes: worktree.write_scope,
        });
    }

    Ok(claims)
}

fn scopes_overlap(left: &str, right: &str) -> bool {
    let left = normalize_scope(left);
    let right = normalize_scope(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    left == right
        || left
            .strip_prefix(&right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(&left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_scope(value: &str) -> String {
    let mut normalized = value.trim();
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped;
    }
    normalized.trim_end_matches('/').to_string()
}

pub(super) fn id_date(id: &str) -> Result<String> {
    validate_id(id)?;
    Ok(id[..10].to_string())
}

pub(super) fn current_date() -> Result<String> {
    let output = Command::new("date")
        .arg("+%F")
        .output()
        .map_err(|err| format!("failed to run date +%F: {err}"))?;
    if !output.status.success() {
        return Err("date +%F failed".into());
    }
    let date = String::from_utf8_lossy(&output.stdout).trim().to_string();
    validate_date(&date)?;
    Ok(date)
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    let Some(date) = id.get(..10) else {
        return Err("id must start with YYYY-MM-DD".into());
    };
    validate_date(date)?;
    if id.as_bytes().get(10) != Some(&b'-') {
        return Err("id must use YYYY-MM-DD-short-kebab-title".into());
    }
    let slug = &id[11..];
    if slug.is_empty() {
        return Err("id slug must not be empty".into());
    }
    if slug.starts_with('-') || slug.ends_with('-') || slug.contains("--") {
        return Err("id slug must be non-empty kebab-case".into());
    }
    if !slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(
            "id slug must contain only lowercase ASCII letters, digits, and hyphens".into(),
        );
    }
    Ok(())
}

fn validate_date(value: &str) -> Result<()> {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !value.chars().all(|ch| ch.is_ascii_digit() || ch == '-')
    {
        return Err("date must be YYYY-MM-DD".into());
    }
    let year = parts[0]
        .parse::<u32>()
        .map_err(|_| "date year must be numeric".to_string())?;
    let month = parts[1]
        .parse::<u32>()
        .map_err(|_| "date month must be numeric".to_string())?;
    let day = parts[2]
        .parse::<u32>()
        .map_err(|_| "date day must be numeric".to_string())?;
    if !(2000..=2099).contains(&year) {
        return Err("date year must be in 2000..=2099".into());
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return Err("date month must be 01..=12".into()),
    };
    if day == 0 || day > max_day {
        return Err(format!("date day must be 01..={max_day:02}"));
    }
    Ok(())
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

pub(super) fn validate_repo_relative_path(path: &str) -> Result<()> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("path must not be empty".into());
    }
    if trimmed != path {
        return Err("path must not have leading or trailing whitespace".into());
    }
    if trimmed.starts_with('-') {
        return Err("path must not start with `-`".into());
    }
    if trimmed.contains('\\') {
        return Err("path must use `/` separators".into());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("path must not contain whitespace".into());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("path must be repo-relative".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => return Err("path must not contain `.`".into()),
            Component::ParentDir => return Err("path must not contain `..`".into()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be repo-relative".into());
            }
        }
    }
    Ok(())
}

fn validate_progress_reference(
    root: &Path,
    file: &Path,
    field: &str,
    path: &str,
    template: bool,
) -> Result<()> {
    validate_repo_relative_path(path)
        .map_err(|err| format!("{}: {field}: {err}", relative(root, file)))?;
    if !path.starts_with("docs/progress/") {
        return Err(format!(
            "{}: {field}: progress reference `{path}` must live under docs/progress/",
            relative(root, file)
        ));
    }
    if !template && !root.join(path).exists() {
        return Err(format!(
            "{}: {field}: reference `{path}` does not exist",
            relative(root, file)
        ));
    }
    Ok(())
}

fn require_nonempty(
    root: &Path,
    file: &Path,
    field: &str,
    value: &str,
    template: bool,
) -> Result<()> {
    if template {
        return Ok(());
    }
    if value.trim().is_empty() {
        Err(format!(
            "{}: {field} must not be empty",
            relative(root, file)
        ))
    } else {
        Ok(())
    }
}

pub(super) fn parse_plan_status(value: &str) -> Result<PlanStatus> {
    match value {
        "proposed" => Ok(PlanStatus::Proposed),
        "active" => Ok(PlanStatus::Active),
        "blocked" => Ok(PlanStatus::Blocked),
        "complete" => Ok(PlanStatus::Complete),
        "canceled" => Ok(PlanStatus::Canceled),
        other => Err(format!(
            "unknown plan status `{other}`, expected proposed, active, blocked, complete, or canceled"
        )),
    }
}

pub(super) fn parse_handoff_status(value: &str) -> Result<HandoffStatus> {
    match value {
        "open" => Ok(HandoffStatus::Open),
        "closed" => Ok(HandoffStatus::Closed),
        other => Err(format!(
            "unknown handoff status `{other}`, expected open or closed"
        )),
    }
}

pub(super) fn parse_worktree_status(value: &str) -> Result<WorktreeStatus> {
    match value {
        "active" => Ok(WorktreeStatus::Active),
        "merged" => Ok(WorktreeStatus::Merged),
        "abandoned" => Ok(WorktreeStatus::Abandoned),
        "closed" => Ok(WorktreeStatus::Closed),
        other => Err(format!(
            "unknown worktree status `{other}`, expected active, merged, abandoned, or closed"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_id_policy_requires_dated_kebab_case() {
        assert!(validate_id("2026-04-27-xtask-progress").is_ok());
        assert!(validate_id("2026-02-29-nope").is_err());
        assert!(validate_id("2026-04-27-Xtask").is_err());
        assert!(validate_id("2026-04-27-").is_err());
    }

    #[test]
    fn progress_path_policy_requires_clean_repo_relative_paths() {
        assert!(validate_repo_relative_path("docs/progress/plans").is_ok());
        assert!(validate_repo_relative_path("/tmp/plan.json").is_err());
        assert!(validate_repo_relative_path("../docs/progress").is_err());
        assert!(validate_repo_relative_path("docs/progress with space").is_err());
    }

    #[test]
    fn progress_scope_overlap_uses_path_prefixes() {
        assert!(scopes_overlap("crates/tx-kernel", "crates/tx-kernel/src"));
        assert!(scopes_overlap("./docs/progress/", "docs/progress"));
        assert!(!scopes_overlap("crates/tx-kernel", "crates/tx-kernelish"));
        assert!(!scopes_overlap("docs/design", "crates/tx-kernel"));
    }

    #[test]
    fn progress_plan_schema_rejects_unknown_fields() {
        let value = serde_json::json!({
            "schema": "tx.progress.plan.v1",
            "id": "2026-04-27-schema-test",
            "title": "Schema test",
            "status": "proposed",
            "created": "2026-04-27",
            "updated": "2026-04-27",
            "owner": "agent",
            "summary": "test",
            "scope": {
                "in": [],
                "out": [],
                "write_sets": []
            },
            "context": {
                "design_docs": [],
                "progress_refs": [],
                "external_refs": []
            },
            "steps": [],
            "verification": [],
            "risks": [],
            "blockers": [],
            "handoff": {
                "needed": false,
                "path": null
            },
            "surprise": true
        });
        assert!(serde_json::from_value::<PlanV1>(value).is_err());
    }

    #[test]
    fn progress_claim_overlap_validation_reads_real_records() {
        let root =
            std::env::temp_dir().join(format!("tx-xtask-progress-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs/progress/plans")).unwrap();
        fs::create_dir_all(root.join("docs/progress/worktrees")).unwrap();

        let plan = PlanV1 {
            schema: ProgressKind::Plan.schema().into(),
            id: "2026-04-27-overlap-plan".into(),
            title: "Overlap plan".into(),
            status: PlanStatus::Active,
            created: "2026-04-27".into(),
            updated: "2026-04-27".into(),
            owner: "agent-a".into(),
            summary: "test".into(),
            scope: ScopeV1 {
                in_scope: Vec::new(),
                out: Vec::new(),
                write_sets: vec![WriteSetV1 {
                    owner: "agent-a".into(),
                    paths: vec!["crates/tx-kernel".into()],
                    notes: String::new(),
                }],
            },
            context: ContextV1 {
                design_docs: Vec::new(),
                progress_refs: Vec::new(),
                external_refs: Vec::new(),
            },
            steps: Vec::new(),
            verification: Vec::new(),
            risks: Vec::new(),
            blockers: Vec::new(),
            handoff: HandoffRefV1 {
                needed: false,
                path: None,
            },
        };
        let worktree_path = root
            .join("docs/progress/worktrees")
            .join("2026-04-27-overlap-worktree.json");
        let mut worktree = WorktreeV1 {
            schema: ProgressKind::Worktree.schema().into(),
            id: "2026-04-27-overlap-worktree".into(),
            created: "2026-04-27".into(),
            updated: "2026-04-27".into(),
            status: WorktreeStatus::Active,
            branch: "codex/overlap".into(),
            path: root.display().to_string(),
            base: "main".into(),
            plan: None,
            owner: "agent-b".into(),
            write_scope: vec!["crates/tx-kernel/src".into()],
            verification: Vec::new(),
            notes: String::new(),
        };

        write_json_value(
            &root
                .join("docs/progress/plans")
                .join("2026-04-27-overlap-plan.json"),
            &plan,
        )
        .unwrap();
        write_json_value(&worktree_path, &worktree).unwrap();
        assert!(validate_claim_overlaps(&root).is_err());

        worktree.write_scope = vec!["crates/tx-reactor".into()];
        write_json_value(&worktree_path, &worktree).unwrap();
        assert!(validate_claim_overlaps(&root).is_ok());

        fs::remove_dir_all(&root).unwrap();
    }
}
