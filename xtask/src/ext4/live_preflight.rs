use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    CrashCutCampaignPlan, CrashCutFamily, Tier1Authorities, XfstestsSourceLock, resolve_repo_path,
    run_workspace, verify_xfstests_source_lock,
};
use crate::Result;
use crate::image;
use crate::target::TxTarget;
use crate::util::{command_exists, command_or_candidates, run_cmd_owned_in};

const XFSTESTS_SOURCE_URL: &str = "https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git";
const DEFAULT_TIER1_EXT4_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const TIER1_ROLE_IMAGE_COPY_COUNT: u64 = 4;
const TIER1_PER_CRASH_CUT_IMAGE_COPY_COUNT: u64 = 2;
const TIER1_COW_PER_IMAGE_WRITE_BUDGET_BYTES: u64 = 1024 * 1024;
const TIER1_STORAGE_MIN_MARGIN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TIER1_STORAGE_MARGIN_DIVISOR: u64 = 20;

#[derive(Clone, Copy, Debug)]
pub(super) struct StorageCapacityEstimate {
    pub(super) available_bytes: u64,
    pub(super) required_bytes: u64,
    pub(super) estimated_image_bytes: u64,
    pub(super) role_image_copy_count: u64,
    pub(super) per_crash_cut_image_copy_count: u64,
    pub(super) cow_clone_supported: bool,
    pub(super) cow_per_image_write_budget_bytes: u64,
}

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
    let xfstests_selected_cases_verified =
        collect_xfstests_preflight(root, authorities, &mut blockers);
    if xfstests_selected_cases_verified {
        xfstests_source_prepared = true;
    }
    let linux_rw_replay_ready = collect_linux_replay_preflight(root, &mut blockers);
    let storage_capacity = collect_storage_capacity_preflight(root, authorities, &mut blockers);
    if let Some(report_path) = report_path {
        write_live_preflight_report(
            root,
            run_id,
            authorities,
            &blockers,
            report_path,
            materialize_xfstests,
            xfstests_source_prepared,
            xfstests_selected_cases_verified,
            linux_rw_replay_ready,
            storage_capacity,
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
    xfstests_selected_cases_verified: bool,
    linux_rw_replay_ready: bool,
    storage_capacity: Option<StorageCapacityEstimate>,
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
    let storage_capacity = storage_capacity
        .map(|estimate| {
            serde_json::json!({
                "available_bytes": estimate.available_bytes,
                "required_bytes": estimate.required_bytes,
                "estimated_image_bytes": estimate.estimated_image_bytes,
                "role_image_copy_count": estimate.role_image_copy_count,
                "per_crash_cut_image_copy_count": estimate.per_crash_cut_image_copy_count,
                "cow_clone_supported": estimate.cow_clone_supported,
                "cow_per_image_write_budget_bytes": estimate.cow_per_image_write_budget_bytes,
                "copy_mode": if estimate.cow_clone_supported { "cow-clone-required" } else { "ordinary-copy-estimate" },
            })
        })
        .unwrap_or(serde_json::Value::Null);
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
            "xfstests_selected_cases_verified": xfstests_selected_cases_verified,
            "linux_rw_replay_ready": linux_rw_replay_ready,
            "storage_capacity": storage_capacity,
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

fn collect_storage_capacity_preflight(
    root: &Path,
    authorities: &Tier1Authorities,
    blockers: &mut Vec<String>,
) -> Option<StorageCapacityEstimate> {
    let available_bytes = match available_storage_bytes(root) {
        Ok(bytes) => bytes,
        Err(err) => {
            blockers.push(format!(
                "failed to inspect free space for live Tier 1 execution: {err}"
            ));
            return None;
        }
    };
    Some(collect_storage_capacity_preflight_for_test(
        available_bytes,
        authorities.crash_cuts.expanded_cut_count as u64,
        tier1_ext4_image_size_estimate(root),
        run_workspace::tier1_image_cow_clone_supported(root),
        blockers,
    ))
}

pub(super) fn collect_storage_capacity_preflight_for_test(
    available_bytes: u64,
    expanded_cut_count: u64,
    estimated_image_bytes: u64,
    cow_clone_supported: bool,
    blockers: &mut Vec<String>,
) -> StorageCapacityEstimate {
    let required_bytes = required_live_tier1_workspace_bytes(
        expanded_cut_count,
        estimated_image_bytes,
        cow_clone_supported,
    );
    let estimate = StorageCapacityEstimate {
        available_bytes,
        required_bytes,
        estimated_image_bytes,
        role_image_copy_count: TIER1_ROLE_IMAGE_COPY_COUNT,
        per_crash_cut_image_copy_count: TIER1_PER_CRASH_CUT_IMAGE_COPY_COUNT,
        cow_clone_supported,
        cow_per_image_write_budget_bytes: TIER1_COW_PER_IMAGE_WRITE_BUDGET_BYTES,
    };
    if available_bytes < required_bytes {
        let copy_mode = if cow_clone_supported {
            format!(
                "CoW clone image staging, reserving {} bytes of divergent writes per cloned image",
                TIER1_COW_PER_IMAGE_WRITE_BUDGET_BYTES
            )
        } else {
            "ordinary image copies; CoW clone support was not proven on this host".into()
        };
        blockers.push(format!(
            "insufficient free space for live Tier 1 crash campaign: available {} bytes, requires at least {} bytes (estimated {} byte ext4 images, {} role/base image copies plus {} retained image copies per crash cut)",
            available_bytes,
            required_bytes,
            estimated_image_bytes,
            TIER1_ROLE_IMAGE_COPY_COUNT,
            TIER1_PER_CRASH_CUT_IMAGE_COPY_COUNT
        ) + &format!("; copy mode: {copy_mode}"));
    }
    estimate
}

fn required_live_tier1_workspace_bytes(
    expanded_cut_count: u64,
    image_bytes: u64,
    cow_clone_supported: bool,
) -> u64 {
    let image_copy_count = TIER1_ROLE_IMAGE_COPY_COUNT
        .saturating_add(expanded_cut_count.saturating_mul(TIER1_PER_CRASH_CUT_IMAGE_COPY_COUNT));
    let estimated_bytes = if cow_clone_supported {
        image_bytes
            .saturating_mul(TIER1_ROLE_IMAGE_COPY_COUNT)
            .saturating_add(TIER1_COW_PER_IMAGE_WRITE_BUDGET_BYTES.saturating_mul(image_copy_count))
    } else {
        image_bytes.saturating_mul(image_copy_count)
    };
    let margin =
        (estimated_bytes / TIER1_STORAGE_MARGIN_DIVISOR).max(TIER1_STORAGE_MIN_MARGIN_BYTES);
    estimated_bytes.saturating_add(margin)
}

fn tier1_ext4_image_size_estimate(root: &Path) -> u64 {
    let built_image = root
        .join("target/images")
        .join(image::busybox_root_ext4_name(TxTarget::Rv64Qemu));
    fs::metadata(built_image)
        .map(|metadata| metadata.len())
        .unwrap_or(DEFAULT_TIER1_EXT4_IMAGE_BYTES)
}

fn available_storage_bytes(path: &Path) -> Result<u64> {
    let output = Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .map_err(|err| format!("failed to run df -Pk {}: {err}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "df -Pk {} failed with status {}",
            path.display(),
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .skip(1)
        .next()
        .ok_or_else(|| format!("df -Pk {} produced no data rows", path.display()))?;
    let available_kib = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| {
            format!(
                "df -Pk {} output is missing Available column",
                path.display()
            )
        })?
        .parse::<u64>()
        .map_err(|err| {
            format!(
                "failed to parse df Available column for {}: {err}",
                path.display()
            )
        })?;
    available_kib
        .checked_mul(1024)
        .ok_or_else(|| format!("df Available column overflow for {}", path.display()))
}

fn collect_xfstests_preflight(
    root: &Path,
    authorities: &Tier1Authorities,
    blockers: &mut Vec<String>,
) -> bool {
    let source_root = authorities.selection.source_lock.root_path(root);
    if !source_root.exists() {
        blockers.push(format!(
            "pinned xfstests source is missing at {}; run --materialize-xfstests before live acceptance",
            source_root.display()
        ));
        return false;
    }
    if let Err(err) = verify_xfstests_source_lock(&source_root, &authorities.selection.source_lock)
    {
        blockers.push(format!("pinned xfstests source is not ready: {err}"));
        return false;
    }
    if let Err(err) = verify_xfstests_selected_cases(&source_root, &authorities.selection.cases) {
        blockers.push(err);
        return false;
    }
    true
}

fn verify_xfstests_selected_cases(xfstests_root: &Path, cases: &[String]) -> Result<()> {
    for case in cases {
        let path = xfstests_root.join("tests").join(case);
        if !path.is_file() {
            return Err(format!(
                "selected xfstests case {case} is missing at {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn collect_linux_replay_preflight(root: &Path, blockers: &mut Vec<String>) -> bool {
    let runner = root.join("tools/ext4/fault_linux_rw_replay.py");
    let output = match Command::new(&runner).arg("--preflight").output() {
        Ok(output) => output,
        Err(err) => {
            blockers.push(format!(
                "failed to run Linux RW replay preflight {}: {err}",
                runner.display()
            ));
            return false;
        }
    };
    if output.status.success() {
        return true;
    }
    let reason = linux_replay_preflight_reason(&output);
    blockers.push(format!("Linux RW replay preflight blocked: {reason}"));
    false
}

fn linux_replay_preflight_reason(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stderr.lines().rev().chain(stdout.lines().rev()) {
        let line = line.trim();
        if let Some(reason) = line.strip_prefix("error: ") {
            return reason.to_string();
        }
        if !line.is_empty() {
            return line.to_string();
        }
    }
    format!("exit {}", output.status)
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
