use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;
use crate::shell_test;
use crate::target::TxTarget;

use super::{
    CrashCutCampaignPlan, CrashCutCatalog, CrashCutFamily, is_real_sha256, read_json, receipt,
    run_workspace, sha256_file,
};

#[derive(Debug)]
pub(crate) struct CrashCutCampaignEvidence {
    pub(crate) summary: receipt::CrashCuts,
    pub(crate) immutable_images: Vec<receipt::E2fsckImageResult>,
}

#[derive(Debug, Clone)]
pub(crate) struct CrashCutOutcome {
    pub(crate) cut_id: String,
    pub(crate) immutable_image_sha256: String,
    pub(crate) replay_serial_sha256: String,
    pub(crate) e2fsck_exit_code: i32,
    pub(crate) replay_exit_code: i32,
}

impl CrashCutCampaignEvidence {
    #[allow(dead_code)]
    pub(crate) fn new(
        summary: receipt::CrashCuts,
        immutable_images: Vec<receipt::E2fsckImageResult>,
    ) -> Result<Self> {
        if immutable_images.len() < summary.completed {
            return Err(format!(
                "crash-cut e2fsck coverage mismatch: completed={} immutable_images={}",
                summary.completed,
                immutable_images.len()
            ));
        }
        Ok(Self {
            summary,
            immutable_images,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn from_outcomes(
        summary: receipt::CrashCuts,
        outcomes: Vec<CrashCutOutcome>,
    ) -> Result<Self> {
        if outcomes.len() < summary.completed {
            return Err(format!(
                "crash-cut outcome coverage mismatch: completed={} outcomes={}",
                summary.completed,
                outcomes.len()
            ));
        }
        let mut immutable_images = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            if !is_real_sha256(&outcome.replay_serial_sha256) {
                return Err(format!(
                    "replay serial evidence for {} is not a real sha256",
                    outcome.cut_id
                ));
            }
            if outcome.e2fsck_exit_code != 0 {
                return Err(format!(
                    "e2fsck failed for {} with exit {}",
                    outcome.cut_id, outcome.e2fsck_exit_code
                ));
            }
            if outcome.replay_exit_code != 0 {
                return Err(format!(
                    "replay failed for {} with exit {}",
                    outcome.cut_id, outcome.replay_exit_code
                ));
            }
            immutable_images.push(receipt::E2fsckImageResult {
                role: outcome.cut_id,
                image_sha256: outcome.immutable_image_sha256,
                exit_code: outcome.e2fsck_exit_code,
            });
        }
        Self::new(summary, immutable_images)
    }

    #[allow(dead_code)]
    pub(crate) fn from_outcome_manifest(path: &Path) -> Result<Self> {
        let value = read_json(path)?;
        let schema = value
            .get("schema")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                format!(
                    "{}: missing schema tx.ext4.crash_cut_outcome_manifest.v1",
                    path.display()
                )
            })?;
        if schema != "tx.ext4.crash_cut_outcome_manifest.v1" {
            return Err(format!(
                "{}: expected schema tx.ext4.crash_cut_outcome_manifest.v1, found {schema}",
                path.display()
            ));
        }
        let completed = required_usize(&value, "completed", path)?;
        let required = required_usize(&value, "required", path)?;
        let families = required_string_array(&value, "families", path)?;
        let outcomes = value
            .get("outcomes")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{}: missing outcomes", path.display()))?
            .iter()
            .map(|entry| CrashCutOutcome::parse(entry, path))
            .collect::<Result<Vec<_>>>()?;
        Self::from_outcomes(
            receipt::CrashCuts {
                completed,
                required,
                families,
            },
            outcomes,
        )
    }
}

impl CrashCutOutcome {
    fn parse(value: &serde_json::Value, path: &Path) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: outcome entries must be objects", path.display()))?;
        Ok(Self {
            cut_id: required_object_string(object, "cut_id", path)?,
            immutable_image_sha256: required_object_string(object, "immutable_image_sha256", path)?,
            replay_serial_sha256: required_object_string(object, "replay_serial_sha256", path)?,
            e2fsck_exit_code: required_object_i32(object, "e2fsck_exit_code", path)?,
            replay_exit_code: required_object_i32(object, "replay_exit_code", path)?,
        })
    }
}

pub(crate) fn run_crash_cut_campaign(
    run: &mut run_workspace::RunWorkspace,
    crash_cuts: &CrashCutCatalog,
) -> Result<CrashCutCampaignEvidence> {
    if let Some(campaign) = &crash_cuts.campaign {
        let manifest = write_crash_cut_execution_manifest(run, crash_cuts, campaign)?;
        run.record_artifact("crash-campaign-plan", manifest.clone())?;
        let outcomes = run.working_dir().join("crash-cut-outcomes.json");
        if outcomes.is_file() {
            run.record_artifact("crash-cut-outcomes", outcomes.clone())?;
            return CrashCutCampaignEvidence::from_outcome_manifest(&outcomes);
        }
        return Err(format!(
            "deterministic crash-cut executor is not implemented; wrote {} and expected outcome manifest {} but refusing to synthesize completed={} across {} families from {}",
            manifest.display(),
            outcomes.display(),
            crash_cuts.expanded_cut_count,
            crash_cuts.families.len(),
            crash_cuts.file.path.display()
        ));
    }
    Err(format!(
        "deterministic crash-cut campaign runner is not implemented; refusing to synthesize completed={} across {} families from {}",
        crash_cuts.expanded_cut_count,
        crash_cuts.families.len(),
        crash_cuts.file.path.display()
    ))
}

pub(crate) fn execute_crash_cut_campaign(
    root: &Path,
    run: &mut run_workspace::RunWorkspace,
    crash_cuts: &CrashCutCatalog,
    boot_image: &Path,
    source_image: &Path,
    workload_image: &Path,
    e2fsck: &str,
) -> Result<CrashCutCampaignEvidence> {
    let Some(campaign) = &crash_cuts.campaign else {
        return Err(format!(
            "deterministic crash-cut campaign runner is not implemented; refusing to synthesize completed={} across {} families from {}",
            crash_cuts.expanded_cut_count,
            crash_cuts.families.len(),
            crash_cuts.file.path.display()
        ));
    };

    let manifest = write_crash_cut_execution_manifest(run, crash_cuts, campaign)?;
    run.record_artifact("crash-campaign-plan", manifest.clone())?;

    let mut outcomes = Vec::with_capacity(crash_cuts.expanded_cut_count);
    let mut families = Vec::new();
    let mut seen_families = std::collections::BTreeSet::new();

    for idx in 0..crash_cuts.expanded_cut_count {
        let family = &crash_cuts.families[idx % crash_cuts.families.len()];
        let phase_marker = family.phase_marker.as_deref().ok_or_else(|| {
            format!(
                "crash family {} missing phase_marker for live execution",
                family.id
            )
        })?;
        if seen_families.insert(family.id.clone()) {
            families.push(family.id.clone());
        }

        let cut_id = format!("crash-cut-{idx:04}");
        let job = write_fault_job_request(
            root,
            run,
            &manifest,
            &cut_id,
            family,
            phase_marker,
            campaign,
            boot_image,
            source_image,
            workload_image,
        )?;
        run_fault_qemu_executor(root, &job.request_path, e2fsck)?;
        let campaign_plan_sha256 = sha256_file(&manifest)?;
        let fault_result = parse_fault_job_result(
            &job.result_path,
            &family.id,
            &cut_id,
            &campaign_plan_sha256,
            &job.crash_image,
            &job.replay_image,
            &job.serial_log,
            phase_marker,
            &job.e2fsck_log,
        )?;

        let replay_args = crash_cut_shell_test_args(
            boot_image,
            &job.replay_image,
            &campaign.replay_script,
            None,
            Some(&job.replay_serial_log),
        );
        let replay_exit_code = match shell_test::shell_test(root, replay_args) {
            Ok(()) => 0,
            Err(err) => {
                println!("{err}");
                1
            }
        };

        let immutable_image_sha256 = sha256_file(&job.replay_image)?;
        let replay_serial_sha256 = sha256_file(&job.replay_serial_log)?;
        outcomes.push(CrashCutOutcome {
            cut_id,
            immutable_image_sha256,
            replay_serial_sha256,
            e2fsck_exit_code: fault_result.e2fsck_exit_code,
            replay_exit_code,
        });
        if fault_result.e2fsck_exit_code != 0 {
            return Err(format!(
                "crash cut {} failed e2fsck with exit {}",
                outcomes
                    .last()
                    .map(|outcome| outcome.cut_id.as_str())
                    .unwrap_or("unknown"),
                fault_result.e2fsck_exit_code
            ));
        }
        if replay_exit_code != 0 {
            return Err(format!(
                "crash cut {} failed replay with exit {}",
                outcomes
                    .last()
                    .map(|outcome| outcome.cut_id.as_str())
                    .unwrap_or("unknown"),
                replay_exit_code
            ));
        }
    }

    let outcomes_path = write_crash_cut_outcome_manifest(
        run,
        crash_cuts.expanded_cut_count,
        crash_cuts.expanded_cut_count,
        families,
        outcomes,
    )?;
    run.record_artifact("crash-cut-outcomes", outcomes_path.clone())?;
    run_crash_cut_campaign(run, crash_cuts)
}

pub(crate) fn crash_cut_shell_test_args(
    boot_image: &Path,
    cut_image: &Path,
    script: &Path,
    stop_after_needle: Option<&str>,
    serial_log: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "--target".into(),
        TxTarget::Rv64Qemu.name().to_string(),
        "--profile".into(),
        "busybox".into(),
        "--script".into(),
        script.display().to_string(),
        "--extra-rv64-ext4".into(),
        boot_image.display().to_string(),
        "--extra-rv64-ext4".into(),
        cut_image.display().to_string(),
    ];
    if let Some(needle) = stop_after_needle {
        args.push("--stop-after-needle".into());
        args.push(needle.into());
    }
    if let Some(serial_log) = serial_log {
        args.push("--serial-log".into());
        args.push(serial_log.display().to_string());
    }
    args
}

#[derive(Debug)]
pub(crate) struct FaultJobPaths {
    pub(crate) request_path: PathBuf,
    pub(crate) result_path: PathBuf,
    pub(crate) crash_image: PathBuf,
    pub(crate) replay_image: PathBuf,
    pub(crate) serial_log: PathBuf,
    pub(crate) replay_serial_log: PathBuf,
    pub(crate) e2fsck_log: PathBuf,
}

#[derive(Debug)]
pub(crate) struct FaultJobResult {
    pub(crate) e2fsck_exit_code: i32,
}

pub(crate) fn write_fault_job_request(
    root: &Path,
    run: &mut run_workspace::RunWorkspace,
    campaign_manifest: &Path,
    cut_id: &str,
    family: &CrashCutFamily,
    phase_marker: &str,
    campaign: &CrashCutCampaignPlan,
    test_image: &Path,
    scratch_image: &Path,
    workload_image: &Path,
) -> Result<FaultJobPaths> {
    let job_dir = run.working_dir().join("crash-cuts").join(cut_id);
    fs::create_dir_all(&job_dir)
        .map_err(|err| format!("failed to create {}: {err}", job_dir.display()))?;
    let request_path = job_dir.join("job-request.json");
    let crash_image = job_dir.join("crash.img");
    let replay_image = job_dir.join("replay.img");
    let result_path = job_dir.join("result.json");
    let executor_plan_path = job_dir.join("executor-plan.json");
    let e2fsck_log = job_dir.join("e2fsck-fn.log");
    let serial_log = job_dir.join("serial.log");
    let replay_serial_log = job_dir.join("replay-serial.log");
    let manifest = serde_json::json!({
        "schema": "tx.ext4.fault_job_request.v1",
        "campaign_plan_sha256": sha256_file(campaign_manifest)?,
        "job": {
            "case": family.id,
            "cut": cut_id,
            "iteration": 1,
            "serial_log": serial_log.display().to_string(),
            "crash_image": crash_image.display().to_string(),
            "replay_image": replay_image.display().to_string(),
            "checks": [
                {
                    "tool": "e2fsck",
                    "args": ["-fn", replay_image.display().to_string()],
                    "log": e2fsck_log.display().to_string()
                }
            ]
        },
        "role_images": {
            "test": test_image.display().to_string(),
            "scratch": scratch_image.display().to_string(),
            "workload": workload_image.display().to_string()
        },
        "qemu": {
            "target": TxTarget::Rv64Qemu.name(),
            "profile": "busybox",
            "script": root.join(&campaign.workload_script).display().to_string(),
            "timeout_ms": 120000,
            "cut_marker": phase_marker
        }
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
    fs::write(&request_path, text)
        .map_err(|err| format!("failed to write {}: {err}", request_path.display()))?;
    run.record_artifact(format!("{cut_id}-job-request"), request_path.clone())?;
    run.record_artifact(
        format!("{cut_id}-executor-plan"),
        executor_plan_path.clone(),
    )?;
    run.record_artifact(format!("{cut_id}-result"), result_path.clone())?;
    run.record_artifact(format!("{cut_id}-serial"), serial_log.clone())?;
    run.record_artifact(format!("{cut_id}-replay-serial"), replay_serial_log.clone())?;
    run.record_artifact(format!("{cut_id}-e2fsck-log"), e2fsck_log.clone())?;
    run.record_artifact(format!("{cut_id}-crash-image"), crash_image.clone())?;
    run.record_artifact(format!("{cut_id}-replay-image"), replay_image.clone())?;
    Ok(FaultJobPaths {
        request_path,
        result_path,
        crash_image,
        replay_image,
        serial_log,
        replay_serial_log,
        e2fsck_log,
    })
}

fn run_fault_qemu_executor(root: &Path, request_path: &Path, e2fsck: &str) -> Result<()> {
    let script = root.join("tools/ext4/fault_qemu_executor.py");
    if !script.is_file() {
        return Err(format!("missing fault executor {}", script.display()));
    }
    println!("$ python3 {} {}", script.display(), request_path.display());
    let status = Command::new("python3")
        .arg(&script)
        .arg(request_path)
        .env("TX_EXT4_FAULT_E2FSCK_COMMAND", e2fsck)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run {}: {err}", script.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "fault executor {} exited with {status}",
            script.display()
        ))
    }
}

pub(crate) fn parse_fault_job_result(
    path: &Path,
    expected_case: &str,
    expected_cut: &str,
    expected_campaign_plan_sha256: &str,
    crash_image: &Path,
    replay_image: &Path,
    serial_log: &Path,
    expected_phase_marker: &str,
    e2fsck_log: &Path,
) -> Result<FaultJobResult> {
    let value = read_json(path)?;
    let schema = value
        .get("schema")
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("{}: missing schema", path.display()))?;
    if schema != "tx.ext4.fault_job_result.v1" {
        return Err(format!(
            "{}: expected schema tx.ext4.fault_job_result.v1, found {schema}",
            path.display()
        ));
    }
    let required_string = |field: &str| {
        value
            .get(field)
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{}: missing {field}", path.display()))
    };
    let campaign_plan_sha256 = required_string("campaign_plan_sha256")?;
    if campaign_plan_sha256 != expected_campaign_plan_sha256 {
        return Err(format!(
            "{}: campaign_plan_sha256 mismatch: expected {}, found {}",
            path.display(),
            expected_campaign_plan_sha256,
            campaign_plan_sha256
        ));
    }
    let case_id = required_string("case")?;
    if case_id != expected_case {
        return Err(format!(
            "{}: case mismatch: expected {expected_case}, found {case_id}",
            path.display()
        ));
    }
    let cut_id = required_string("cut")?;
    if cut_id != expected_cut {
        return Err(format!(
            "{}: cut mismatch: expected {expected_cut}, found {cut_id}",
            path.display()
        ));
    }
    let hard_kill = value
        .get("hard_kill_observed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !hard_kill {
        return Err(format!(
            "{}: hard_kill_observed is not true",
            path.display()
        ));
    }
    let replay_attempted = value
        .get("replay_attempted")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !replay_attempted {
        return Err(format!("{}: replay_attempted is not true", path.display()));
    }
    let serial = fs::read_to_string(serial_log).map_err(|err| {
        format!(
            "{}: failed to read serial log {}: {err}",
            path.display(),
            serial_log.display()
        )
    })?;
    if !serial.contains(expected_phase_marker) {
        return Err(format!(
            "{}: serial log {} missing phase marker {}",
            path.display(),
            serial_log.display(),
            expected_phase_marker
        ));
    }
    let e2fsck_exit_code = value
        .get("e2fsck_exit")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| format!("{}: missing e2fsck_exit", path.display()))
        .and_then(|raw| {
            i32::try_from(raw).map_err(|_| format!("{}: e2fsck_exit out of range", path.display()))
        })?;
    if e2fsck_exit_code != 0 {
        return Err(format!(
            "{}: e2fsck_exit must be 0, found {e2fsck_exit_code}",
            path.display()
        ));
    }

    let checks = value
        .get("e2fsck_checks")
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing e2fsck_checks", path.display()))?;
    if checks.is_empty() {
        return Err(format!("{}: e2fsck_checks is empty", path.display()));
    }
    let replay_arg = replay_image.display().to_string();
    let e2fsck_log_path = e2fsck_log.display().to_string();
    for check in checks {
        let object = check
            .as_object()
            .ok_or_else(|| format!("{}: e2fsck check must be an object", path.display()))?;
        if object.get("tool").and_then(|value| value.as_str()) != Some("e2fsck") {
            return Err(format!(
                "{}: e2fsck check tool must be e2fsck",
                path.display()
            ));
        }
        let args = object
            .get("args")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{}: e2fsck check is missing args", path.display()))?;
        let args = args
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{}: e2fsck args must be strings", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        if args != ["-fn", replay_arg.as_str()] {
            return Err(format!(
                "{}: e2fsck check must run -fn against {}",
                path.display(),
                replay_image.display()
            ));
        }
        if object.get("log").and_then(|value| value.as_str()) != Some(e2fsck_log_path.as_str()) {
            return Err(format!(
                "{}: e2fsck check log must be {}",
                path.display(),
                e2fsck_log.display()
            ));
        }
        let log_sha256 = object
            .get("log_sha256")
            .and_then(|value| value.as_str())
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| format!("{}: e2fsck check has invalid log_sha256", path.display()))?;
        if !e2fsck_log.is_file() {
            return Err(format!(
                "{}: e2fsck log is missing: {}",
                path.display(),
                e2fsck_log.display()
            ));
        }
        if sha256_file(e2fsck_log)? != log_sha256 {
            return Err(format!(
                "{}: e2fsck log sha256 mismatch for {}",
                path.display(),
                e2fsck_log.display()
            ));
        }
        let exit_code = object
            .get("exit_code")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| format!("{}: e2fsck check is missing exit_code", path.display()))
            .and_then(|raw| {
                i32::try_from(raw)
                    .map_err(|_| format!("{}: e2fsck check exit_code out of range", path.display()))
            })?;
        if exit_code != 0 {
            return Err(format!(
                "{}: e2fsck check exit_code must be 0, found {exit_code}",
                path.display()
            ));
        }
    }
    if !crash_image.is_file() {
        return Err(format!(
            "{}: crash image is missing: {}",
            path.display(),
            crash_image.display()
        ));
    }
    if !replay_image.is_file() {
        return Err(format!(
            "{}: replay image is missing: {}",
            path.display(),
            replay_image.display()
        ));
    }
    Ok(FaultJobResult { e2fsck_exit_code })
}

fn write_crash_cut_outcome_manifest(
    run: &run_workspace::RunWorkspace,
    required: usize,
    completed: usize,
    families: Vec<String>,
    outcomes: Vec<CrashCutOutcome>,
) -> Result<PathBuf> {
    let manifest = serde_json::json!({
        "schema": "tx.ext4.crash_cut_outcome_manifest.v1",
        "completed": completed,
        "required": required,
        "families": families,
        "outcomes": outcomes.iter().map(|outcome| serde_json::json!({
            "cut_id": &outcome.cut_id,
            "immutable_image_sha256": &outcome.immutable_image_sha256,
            "replay_serial_sha256": &outcome.replay_serial_sha256,
            "e2fsck_exit_code": outcome.e2fsck_exit_code,
            "replay_exit_code": outcome.replay_exit_code
        })).collect::<Vec<_>>()
    });
    let path = run.working_dir().join("crash-cut-outcomes.json");
    let text = serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
    fs::write(&path, text).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

fn write_crash_cut_execution_manifest(
    run: &run_workspace::RunWorkspace,
    crash_cuts: &CrashCutCatalog,
    campaign: &CrashCutCampaignPlan,
) -> Result<PathBuf> {
    let cuts = (0..crash_cuts.expanded_cut_count)
        .map(|idx| {
            let family = &crash_cuts.families[idx % crash_cuts.families.len()];
            let phase_marker = family
                .phase_marker
                .as_deref()
                .ok_or_else(|| format!("crash family {} missing phase_marker", family.id))?;
            Ok(serde_json::json!({
                "id": format!("crash-cut-{idx:04}"),
                "index": idx,
                "family": &family.id,
                "phase_marker": phase_marker,
                "workload_script": campaign.workload_script.display().to_string(),
                "workload_script_sha256": &campaign.workload_script_sha256,
                "replay_script": campaign.replay_script.display().to_string(),
                "replay_script_sha256": &campaign.replay_script_sha256,
                "kill_policy": &campaign.kill_policy,
                "e2fsck_mode": &campaign.e2fsck_mode,
                "immutable_image": format!("crash-cut-{idx:04}.img")
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let families = crash_cuts
        .families
        .iter()
        .map(|family| {
            Ok(serde_json::json!({
                "id": &family.id,
                "phase_marker": family
                    .phase_marker
                    .as_deref()
                    .ok_or_else(|| format!("crash family {} missing phase_marker", family.id))?
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = serde_json::json!({
        "schema": "tx.ext4.crash_cut_execution_manifest.v1",
        "expanded_cut_count": crash_cuts.expanded_cut_count,
        "families": families,
        "workload_script": campaign.workload_script.display().to_string(),
        "workload_script_sha256": &campaign.workload_script_sha256,
        "replay_script": campaign.replay_script.display().to_string(),
        "replay_script_sha256": &campaign.replay_script_sha256,
        "kill_policy": &campaign.kill_policy,
        "e2fsck_mode": &campaign.e2fsck_mode,
        "cuts": cuts
    });
    let path = run.working_dir().join("crash-campaign-plan.json");
    let text = serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
    fs::write(&path, text).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

fn required_usize(value: &serde_json::Value, key: &str, path: &Path) -> Result<usize> {
    let Some(raw) = value.get(key).and_then(|value| value.as_u64()) else {
        return Err(format!("{}: missing {key}", path.display()));
    };
    usize::try_from(raw).map_err(|_| format!("{}: {key} is too large", path.display()))
}

fn required_string_array(value: &serde_json::Value, key: &str, path: &Path) -> Result<Vec<String>> {
    value
        .get(key)
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing {key}", path.display()))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "{}: {key} entries must be non-empty strings",
                        path.display()
                    )
                })
        })
        .collect()
}

fn required_object_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{}: missing outcome.{key}", path.display()))
}

fn required_object_i32(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<i32> {
    let Some(raw) = object.get(key).and_then(|value| value.as_i64()) else {
        return Err(format!("{}: missing outcome.{key}", path.display()));
    };
    i32::try_from(raw).map_err(|_| format!("{}: outcome.{key} is out of range", path.display()))
}
