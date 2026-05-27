use std::path::Path;

use serde_json::json;

use super::*;
use crate::util::{option_value, optional_option_value, relative};
use crate::Result;

use super::validate::*;

pub(crate) fn progress(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("progress command needs validate, list, new, claim, or close".into());
    };
    match kind.as_str() {
        "validate" => progress_validate(root),
        "list" => progress_list(root, &args[1..]),
        "new" => progress_new(root, &args[1..]),
        "claim" => progress_claim(root, &args[1..]),
        "close" => progress_close(root, &args[1..]),
        other => Err(format!(
            "unknown progress command '{other}', expected validate, list, new, claim, or close"
        )),
    }
}

fn progress_list(root: &Path, args: &[String]) -> Result<()> {
    let Some(kind_value) = args.first() else {
        return Err("progress list needs plans, handoffs, worktrees, or all".into());
    };
    let as_json = args.iter().any(|arg| arg == "--json");
    let kinds = if kind_value == "all" {
        vec![
            ProgressKind::Plan,
            ProgressKind::Handoff,
            ProgressKind::Worktree,
        ]
    } else {
        vec![ProgressKind::parse(kind_value)?]
    };

    let mut summaries = Vec::new();
    for kind in kinds {
        for file in progress_record_files(root, kind)? {
            summaries.push(progress_summary(root, kind, &file)?);
        }
    }
    summaries.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| a.kind.label().cmp(b.kind.label()))
    });

    if as_json {
        let rows = summaries
            .iter()
            .map(|summary| {
                json!({
                    "kind": summary.kind.label(),
                    "id": summary.id,
                    "status": summary.status,
                    "owner": summary.owner,
                    "title": summary.title,
                    "path": summary.path,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|err| err.to_string())?
        );
    } else if summaries.is_empty() {
        println!("progress: no records");
    } else {
        for summary in summaries {
            println!(
                "• {} {} status={} owner={} path={}",
                summary.kind.label(),
                summary.id,
                summary.status,
                summary.owner,
                summary.path
            );
            if !summary.title.is_empty() {
                println!("  {}", summary.title);
            }
        }
    }
    Ok(())
}

fn progress_new(root: &Path, args: &[String]) -> Result<()> {
    let Some(kind_value) = args.first() else {
        return Err("progress new needs plan, handoff, or worktree".into());
    };
    let kind = ProgressKind::parse(kind_value)?;
    let id = option_value(&args[1..], "--id")?;
    let title = option_value(&args[1..], "--title")?;
    validate_id(&id)?;
    let date = id_date(&id)?;
    let path = progress_record_path(root, kind, &id);
    if path.exists() {
        return Err(format!("{} already exists", relative(root, &path)));
    }

    let owner = optional_option_value(&args[1..], "--owner").unwrap_or_else(|| "unclaimed".into());
    let scopes = option_values(&args[1..], "--scope");
    let text = match kind {
        ProgressKind::Plan => {
            let summary =
                optional_option_value(&args[1..], "--summary").unwrap_or_else(|| title.clone());
            let plan = PlanV1 {
                schema: kind.schema().into(),
                id: id.clone(),
                title,
                status: PlanStatus::Proposed,
                created: date.clone(),
                updated: date,
                owner,
                summary,
                scope: ScopeV1 {
                    in_scope: scopes,
                    out: Vec::new(),
                    write_sets: Vec::new(),
                },
                context: ContextV1 {
                    design_docs: vec!["docs/design/INDEX.md".into()],
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
            validate_plan(root, &path, &plan, false)?;
            serde_json::to_string_pretty(&plan).map_err(|err| err.to_string())?
        }
        ProgressKind::Handoff => {
            let from = optional_option_value(&args[1..], "--from").unwrap_or(owner);
            let handoff = HandoffV1 {
                schema: kind.schema().into(),
                id: id.clone(),
                title,
                created: date,
                from,
                status: HandoffStatus::Open,
                plan: optional_option_value(&args[1..], "--plan"),
                current_state: Vec::new(),
                changed_files: Vec::new(),
                verification: Vec::new(),
                next_actions: Vec::new(),
                caveats: Vec::new(),
                links: Vec::new(),
            };
            validate_handoff(root, &path, &handoff, false)?;
            serde_json::to_string_pretty(&handoff).map_err(|err| err.to_string())?
        }
        ProgressKind::Worktree => {
            let branch = option_value(&args[1..], "--branch")?;
            let worktree_path = option_value(&args[1..], "--path")?;
            if !scopes.is_empty() {
                reject_claim_overlap(root, kind, &id, &owner, &scopes)?;
            }
            let worktree = WorktreeV1 {
                schema: kind.schema().into(),
                id: id.clone(),
                created: date.clone(),
                updated: date,
                status: WorktreeStatus::Active,
                branch,
                path: worktree_path,
                base: optional_option_value(&args[1..], "--base").unwrap_or_else(|| "main".into()),
                plan: optional_option_value(&args[1..], "--plan"),
                owner,
                write_scope: scopes,
                verification: option_values(&args[1..], "--verify"),
                notes: optional_option_value(&args[1..], "--notes").unwrap_or_default(),
            };
            validate_worktree(root, &path, &worktree, false)?;
            serde_json::to_string_pretty(&worktree).map_err(|err| err.to_string())?
        }
    };

    write_new_json(root, &path, &text)?;
    validate_progress_file(root, &path, false)?;
    validate_claim_overlaps(root)?;
    println!("wrote {}", relative(root, &path));
    Ok(())
}

fn progress_claim(root: &Path, args: &[String]) -> Result<()> {
    let Some(kind_value) = args.first() else {
        return Err("progress claim needs plan or worktree".into());
    };
    let kind = ProgressKind::parse(kind_value)?;
    if kind == ProgressKind::Handoff {
        return Err("handoffs are not claimable; claim a plan or worktree".into());
    }
    let id = option_value(&args[1..], "--id")?;
    let owner = option_value(&args[1..], "--owner")?;
    let scopes = option_values(&args[1..], "--scope");
    if scopes.is_empty() {
        return Err("progress claim needs at least one --scope PATH".into());
    }
    for scope in &scopes {
        validate_repo_relative_path(scope)?;
    }

    reject_claim_overlap(root, kind, &id, &owner, &scopes)?;
    let path = progress_record_path(root, kind, &id);
    match kind {
        ProgressKind::Plan => {
            let mut plan: PlanV1 = read_progress_json(&path)?;
            plan.owner = owner.clone();
            if plan.status == PlanStatus::Proposed {
                plan.status = PlanStatus::Active;
            }
            plan.updated = current_date()?;
            plan.scope.write_sets.push(WriteSetV1 {
                owner,
                paths: scopes,
                notes: optional_option_value(&args[1..], "--notes").unwrap_or_default(),
            });
            write_json_value(&path, &plan)?;
        }
        ProgressKind::Worktree => {
            let mut worktree: WorktreeV1 = read_progress_json(&path)?;
            worktree.owner = owner;
            worktree.updated = current_date()?;
            for scope in scopes {
                if !worktree.write_scope.contains(&scope) {
                    worktree.write_scope.push(scope);
                }
            }
            write_json_value(&path, &worktree)?;
        }
        ProgressKind::Handoff => unreachable!(),
    }

    validate_progress_file(root, &path, false)?;
    validate_claim_overlaps(root)?;
    println!("claimed {}", relative(root, &path));
    Ok(())
}

fn progress_close(root: &Path, args: &[String]) -> Result<()> {
    let Some(kind_value) = args.first() else {
        return Err("progress close needs plan, handoff, or worktree".into());
    };
    let kind = ProgressKind::parse(kind_value)?;
    let id = option_value(&args[1..], "--id")?;
    let status = option_value(&args[1..], "--status")?;
    let path = progress_record_path(root, kind, &id);

    match kind {
        ProgressKind::Plan => {
            let mut plan: PlanV1 = read_progress_json(&path)?;
            plan.status = parse_plan_status(&status)?;
            plan.updated = current_date()?;
            write_json_value(&path, &plan)?;
        }
        ProgressKind::Handoff => {
            let mut handoff: HandoffV1 = read_progress_json(&path)?;
            handoff.status = parse_handoff_status(&status)?;
            write_json_value(&path, &handoff)?;
        }
        ProgressKind::Worktree => {
            let mut worktree: WorktreeV1 = read_progress_json(&path)?;
            worktree.status = parse_worktree_status(&status)?;
            worktree.updated = current_date()?;
            write_json_value(&path, &worktree)?;
        }
    }
    validate_progress_file(root, &path, false)?;
    validate_claim_overlaps(root)?;
    println!("updated {}", relative(root, &path));
    Ok(())
}
