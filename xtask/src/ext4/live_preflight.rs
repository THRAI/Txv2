use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    CrashCutCampaignPlan, CrashCutFamily, Tier1Authorities, XfstestsSourceLock, resolve_repo_path,
    verify_xfstests_source_lock,
};
use crate::Result;
use crate::util::{command_exists, command_or_candidates, run_cmd_owned_in};

const XFSTESTS_SOURCE_URL: &str = "https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git";

pub(super) fn run_live_preflight(
    root: &Path,
    run_id: &str,
    report_path: Option<&Path>,
    materialize_xfstests: bool,
    authorities: &Tier1Authorities,
) -> Result<()> {
    let mut blockers = Vec::new();
    let mut xfstests_source_prepared = false;
    println!("ext4 tier1: live-preflight");
    collect_authority_preflight(authorities, &mut blockers);
    collect_tool_preflight(root, authorities, &mut blockers);
    if materialize_xfstests {
        match materialize_xfstests_source(root, &authorities.selection.source_lock) {
            Ok(path) => {
                xfstests_source_prepared = true;
                println!(
                    "ext4 tier1: live-preflight xfstests source ready {}",
                    path.display()
                );
            }
            Err(err) => blockers.push(format!(
                "failed to materialize pinned xfstests source: {err}"
            )),
        }
    }
    collect_xfstests_preflight(root, authorities, &mut blockers);
    collect_linux_replay_preflight(&mut blockers);
    if let Some(report_path) = report_path {
        write_live_preflight_report(
            root,
            run_id,
            authorities,
            &blockers,
            report_path,
            materialize_xfstests,
            xfstests_source_prepared,
        )?;
        println!(
            "ext4 tier1: live-preflight report {}",
            report_path.display()
        );
    }
    if blockers.is_empty() {
        println!("ext4 tier1: live-preflight ok");
        Ok(())
    } else {
        for blocker in &blockers {
            println!("ext4 tier1: live-preflight blocker: {blocker}");
        }
        Err(format!(
            "live ext4 Tier 1 preflight blocked: {}",
            blockers.join("; ")
        ))
    }
}

fn materialize_xfstests_source(root: &Path, source_lock: &XfstestsSourceLock) -> Result<PathBuf> {
    if !command_exists("git") {
        return Err("git is required to materialize the pinned xfstests source".into());
    }
    let preferred = source_lock.root_path(root);
    if preferred.join("check").is_file() {
        verify_xfstests_source_lock(&preferred, source_lock)?;
        return Ok(preferred);
    }
    if preferred.exists() {
        return Err(format!(
            "source path {} exists but is missing check script",
            preferred.display()
        ));
    }
    let parent = preferred
        .parent()
        .ok_or_else(|| format!("source path {} has no parent", preferred.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    let temp = parent.join(".xfstests.tmp");
    if temp.exists() {
        fs::remove_dir_all(&temp)
            .map_err(|err| format!("failed to remove stale {}: {err}", temp.display()))?;
    }
    run_cmd_owned_in(
        parent,
        "git",
        &[
            "clone".into(),
            "--no-checkout".into(),
            XFSTESTS_SOURCE_URL.into(),
            temp.display().to_string(),
        ],
    )?;
    run_cmd_owned_in(
        &temp,
        "git",
        &[
            "checkout".into(),
            "--detach".into(),
            source_lock.revision.clone(),
        ],
    )?;
    verify_xfstests_source_lock(&temp, source_lock)?;
    fs::rename(&temp, &preferred).map_err(|err| {
        format!(
            "failed to publish pinned xfstests source {} -> {}: {err}",
            temp.display(),
            preferred.display()
        )
    })?;
    Ok(preferred)
}

fn write_live_preflight_report(
    root: &Path,
    run_id: &str,
    authorities: &Tier1Authorities,
    blockers: &[String],
    report_path: &Path,
    materialize_xfstests: bool,
    xfstests_source_prepared: bool,
) -> Result<()> {
    let path = resolve_repo_path(root, report_path.to_path_buf());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let generated_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let value = serde_json::json!({
        "schema": "tx.ext4.tier1_live_preflight.v1",
        "run_id": run_id,
        "generated_unix_ms": generated_unix_ms,
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "authorities": {
            "capability_ledger": {
                "path": authorities.capability.path.display().to_string(),
                "sha256": authorities.capability.sha256,
            },
            "xfstests_selection": {
                "path": authorities.selection.file.path.display().to_string(),
                "sha256": authorities.selection.sha256(),
                "status": authorities.selection.status,
                "selected_count": authorities.selection.case_count,
                "source_lock": {
                    "path": authorities.selection.source_lock.path.display().to_string(),
                    "revision": authorities.selection.source_lock.revision,
                    "check_sha256": authorities.selection.source_lock.check_sha256,
                },
            },
            "crash_cut_catalog": {
                "path": authorities.crash_cuts.file.path.display().to_string(),
                "sha256": authorities.crash_cuts.sha256(),
                "status": authorities.crash_cuts.status,
                "expanded_cut_count": authorities.crash_cuts.expanded_cut_count,
                "campaign_declared": authorities.crash_cuts.campaign.is_some(),
            },
            "shell_scenario": {
                "path": authorities.shell_scenario.path.display().to_string(),
                "sha256": authorities.shell_scenario.sha256,
            },
        },
        "preflight": {
            "materialize_xfstests_requested": materialize_xfstests,
            "xfstests_source_prepared": xfstests_source_prepared,
        },
        "result": {
            "ready": blockers.is_empty(),
            "blocker_count": blockers.len(),
            "blockers": blockers,
            "acceptance_receipt_generated": false,
        },
    });
    let text = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("failed to encode live preflight report: {err}"))?;
    fs::write(&path, text + "\n")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn collect_authority_preflight(authorities: &Tier1Authorities, blockers: &mut Vec<String>) {
    if let Some(blocker) = authorities.selection.live_acceptance_blocker() {
        blockers.push(blocker);
    }
    if let Some(blocker) = authorities.crash_cuts.live_acceptance_blocker() {
        blockers.push(blocker);
    }
}

fn collect_tool_preflight(root: &Path, authorities: &Tier1Authorities, blockers: &mut Vec<String>) {
    for tool in ["cargo", "git", "python3", "qemu-system-riscv64"] {
        if !command_exists(tool) {
            blockers.push(format!("{tool} is required for live Tier 1 execution"));
        }
    }
    if command_or_candidates(
        "e2fsck",
        &[
            "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
            "/opt/homebrew/sbin/e2fsck",
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/e2fsck",
            "/usr/local/opt/e2fsprogs/sbin/e2fsck",
            "/usr/local/sbin/e2fsck",
        ],
    )
    .is_none()
    {
        blockers.push("e2fsck is required for live Tier 1 execution".into());
    }
    if command_or_candidates(
        "debugfs",
        &[
            "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
            "/opt/homebrew/sbin/debugfs",
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs",
            "/usr/local/opt/e2fsprogs/sbin/debugfs",
            "/usr/local/sbin/debugfs",
        ],
    )
    .is_none()
    {
        blockers.push("debugfs is required for semantic oracle verification".into());
    }
    for relative in [
        "tools/ext4/fault_qemu_executor.py",
        "tools/ext4/fault_linux_rw_replay.py",
        "tools/ext4/fault_tx_remount.py",
        "tools/ext4/fault_semantic_oracle.py",
    ] {
        let path = root.join(relative);
        if !path_is_executable(&path) {
            blockers.push(format!(
                "repository runner is not executable: {}",
                path.display()
            ));
        }
    }
    if let Some(campaign) = &authorities.crash_cuts.campaign {
        if let Err(err) = verify_campaign_markers(root, &authorities.crash_cuts.families, campaign)
        {
            blockers.push(err);
        }
    }
}

fn collect_xfstests_preflight(
    root: &Path,
    authorities: &Tier1Authorities,
    blockers: &mut Vec<String>,
) {
    let source_root = authorities.selection.source_lock.root_path(root);
    if !source_root.exists() {
        blockers.push(format!(
            "pinned xfstests source is missing at {}; run --materialize-xfstests before live acceptance",
            source_root.display()
        ));
        return;
    }
    if let Err(err) = verify_xfstests_source_lock(&source_root, &authorities.selection.source_lock)
    {
        blockers.push(format!("pinned xfstests source is not ready: {err}"));
    }
}

fn collect_linux_replay_preflight(blockers: &mut Vec<String>) {
    if std::env::consts::OS != "linux" {
        blockers.push(format!(
            "Linux RW replay requires a Linux host; current host is {}",
            std::env::consts::OS
        ));
        return;
    }
    match current_uid() {
        Ok(0) => {}
        Ok(uid) => blockers.push(format!(
            "Linux RW replay requires root for a loop mount; current uid is {uid}"
        )),
        Err(err) => blockers.push(err),
    }
    for tool in ["mount", "umount"] {
        if !command_exists(tool) {
            blockers.push(format!("Linux RW replay requires tool: {tool}"));
        }
    }
}

fn current_uid() -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|err| format!("failed to run id -u for live preflight: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "id -u failed during live preflight with {}",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<u32>()
        .map_err(|err| format!("failed to parse id -u output `{}`: {err}", text.trim()))
}

fn path_is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn verify_campaign_markers(
    root: &Path,
    families: &[CrashCutFamily],
    campaign: &CrashCutCampaignPlan,
) -> Result<()> {
    let script = root.join(&campaign.workload_script);
    let text = fs::read_to_string(&script).map_err(|err| {
        format!(
            "failed to read crash workload script {}: {err}",
            script.display()
        )
    })?;
    for family in families {
        let Some(marker) = &family.phase_marker else {
            return Err(format!(
                "crash family {} is missing a phase marker",
                family.id
            ));
        };
        if !text.contains(marker) {
            return Err(format!(
                "crash workload script {} does not contain phase marker {}",
                script.display(),
                marker
            ));
        }
    }
    Ok(())
}
