use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Result;
use crate::full_build;
use crate::image;
use crate::shell_test;
use crate::target::TxTarget;
use crate::util::{
    command_exists, command_or_candidates, optional_option_value, run_cmd_owned_in, shell_join,
};

mod receipt;
mod run_workspace;

const G0_EXT4_LINTS: &[&str] = &[
    "ext4-lifecycle-ownership",
    "ext4-no-direct-home-write",
    "ext4-durability-flags",
];

#[cfg(test)]
mod tests;

pub(crate) fn ext4(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err("ext4 command needs subcommand: tier1".into());
    };
    match subcommand {
        "tier1" => tier1(root, &args[1..]),
        other => Err(format!("unknown ext4 subcommand '{other}', expected tier1")),
    }
}

fn tier1(root: &Path, args: &[String]) -> Result<()> {
    let invocation = parse_tier1_args(root, args)?;
    if invocation.dry_run {
        invocation.print_dry_run();
        return Ok(());
    }
    invocation.authorities.ensure_live_acceptance_ready()?;

    let mut run = run_workspace::RunWorkspace::create(root, &invocation.run_id)?;
    run.record_authority_inputs(&invocation.authorities.as_input_summary())?;
    run_live_tier1(root, invocation, &mut run)
}

fn run_live_tier1(
    root: &Path,
    invocation: Tier1Invocation,
    run: &mut run_workspace::RunWorkspace,
) -> Result<()> {
    let action_log = invocation.planned_actions();
    let base_target = TxTarget::Rv64Qemu;
    let e2fsck = command_or_candidates(
        "e2fsck",
        &[
            "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
            "/opt/homebrew/sbin/e2fsck",
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/e2fsck",
            "/usr/local/opt/e2fsprogs/sbin/e2fsck",
            "/usr/local/sbin/e2fsck",
        ],
    )
    .ok_or_else(|| "e2fsck is required for the live Tier 1 runner".to_string())?;

    for tool in ["git"] {
        if !command_exists(tool) {
            return Err(format!("{tool} is required for the live Tier 1 runner"));
        }
    }

    run_g0_lints(root)?;

    full_build::full_build(
        root,
        vec![
            "--target".into(),
            base_target.name().to_string(),
            "--skip-doctor".into(),
        ],
    )?;
    image::image(
        root,
        vec![
            "ext4".into(),
            "--profile".into(),
            "busybox".into(),
            "--target".into(),
            base_target.name().to_string(),
        ],
    )?;

    let base_image = root
        .join("target")
        .join("images")
        .join(image::busybox_root_ext4_name(base_target));
    if !base_image.is_file() {
        return Err(format!(
            "missing busybox ext4 image {}",
            base_image.display()
        ));
    }

    let test_image = run.stage_copy("test-image", &base_image, "test.img")?;
    let scratch_image = run.stage_copy("scratch-image", &base_image, "scratch.img")?;
    let workload_image = run.stage_copy("workload-image", &base_image, "workload.img")?;

    let scenario = root.join("tools/shell-tests/ext4-tier1.scn");
    shell_test::shell_test(
        root,
        tier1_shell_test_args(
            base_target,
            &scenario,
            &test_image,
            &scratch_image,
            &workload_image,
        ),
    )?;

    let crash_campaign = execute_crash_cut_campaign(
        root,
        run,
        &invocation.authorities.crash_cuts,
        &test_image,
        &scratch_image,
        &workload_image,
        &e2fsck,
    )?;
    let mut e2fsck_results = Vec::new();
    for (role, path) in [
        ("test", &test_image),
        ("scratch", &scratch_image),
        ("workload", &workload_image),
    ] {
        let (exit_code, output) =
            run_capture(root, &e2fsck, &["-fn".into(), path.display().to_string()])?;
        let log_path = run.working_dir().join(format!("e2fsck-{role}.log"));
        fs::write(&log_path, &output)
            .map_err(|err| format!("failed to write {}: {err}", log_path.display()))?;
        run.record_artifact(format!("e2fsck-{role}-log"), log_path)?;
        e2fsck_results.push(receipt::E2fsckImageResult {
            role: role.into(),
            image_sha256: sha256_file(path)?,
            exit_code,
        });
        if !output.trim().is_empty() {
            println!("{output}");
        }
    }
    e2fsck_results.extend(crash_campaign.immutable_images);
    let e2fsck_failures = e2fsck_results
        .iter()
        .filter(|image| image.exit_code != 0)
        .count();
    let xfstests_summary = run_xfstests_selection(root, run, &invocation.authorities)?;
    let role_images = receipt::RoleImages {
        test: receipt::RoleImage {
            path: test_image.display().to_string(),
            sha256: sha256_file(&test_image)?,
        },
        scratch: receipt::RoleImage {
            path: scratch_image.display().to_string(),
            sha256: sha256_file(&scratch_image)?,
        },
        workload: receipt::RoleImage {
            path: workload_image.display().to_string(),
            sha256: sha256_file(&workload_image)?,
        },
    };
    let commit = git_head(root)?;
    let mut notes = vec![
        "live Tier 1 shell matrix executed".into(),
        "xfstests root still resolved from the pinned source mirror if absent".into(),
    ];
    if e2fsck_failures != 0 {
        notes.push(format!("e2fsck failures={e2fsck_failures}"));
    }
    let receipt = receipt::Tier1AcceptanceReceipt::from_live(
        &invocation.run_id,
        commit,
        invocation.authorities.as_input_summary(),
        role_images,
        crash_campaign.summary,
        receipt::E2fsckSummary {
            immutable_images: e2fsck_results,
            failures: e2fsck_failures,
        },
        xfstests_summary,
        &action_log,
        &notes,
    );
    let receipt_path = run.finalize_with_receipt(receipt)?;
    println!("ext4 tier1: wrote {}", receipt_path.display());
    Ok(())
}

fn tier1_shell_test_args(
    target: TxTarget,
    scenario: &Path,
    test_image: &Path,
    scratch_image: &Path,
    workload_image: &Path,
) -> Vec<String> {
    vec![
        "--target".into(),
        target.name().to_string(),
        "--profile".into(),
        "busybox".into(),
        "--script".into(),
        scenario.display().to_string(),
        "--extra-rv64-ext4".into(),
        test_image.display().to_string(),
        "--extra-rv64-ext4".into(),
        scratch_image.display().to_string(),
        "--extra-rv64-ext4".into(),
        workload_image.display().to_string(),
    ]
}

#[derive(Debug)]
struct CrashCutCampaignEvidence {
    summary: receipt::CrashCuts,
    immutable_images: Vec<receipt::E2fsckImageResult>,
}

#[derive(Debug, Clone)]
struct CrashCutOutcome {
    cut_id: String,
    immutable_image_sha256: String,
    replay_serial_sha256: String,
    e2fsck_exit_code: i32,
    replay_exit_code: i32,
}

impl CrashCutCampaignEvidence {
    #[allow(dead_code)]
    fn new(
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
    fn from_outcomes(summary: receipt::CrashCuts, outcomes: Vec<CrashCutOutcome>) -> Result<Self> {
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
    fn from_outcome_manifest(path: &Path) -> Result<Self> {
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

fn is_real_sha256(value: &str) -> bool {
    value.len() == 64
        && value.chars().all(|ch| ch.is_ascii_hexdigit())
        && value.chars().any(|ch| ch != '0')
}

fn run_crash_cut_campaign(
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

fn execute_crash_cut_campaign(
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

fn crash_cut_shell_test_args(
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
struct FaultJobPaths {
    request_path: PathBuf,
    result_path: PathBuf,
    crash_image: PathBuf,
    replay_image: PathBuf,
    serial_log: PathBuf,
    replay_serial_log: PathBuf,
    e2fsck_log: PathBuf,
}

#[derive(Debug)]
struct FaultJobResult {
    e2fsck_exit_code: i32,
}

fn write_fault_job_request(
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

fn parse_fault_job_result(
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

#[derive(Debug)]
struct Tier1Invocation {
    run_id: String,
    dry_run: bool,
    authorities: Tier1Authorities,
}

impl Tier1Invocation {
    fn print_dry_run(&self) {
        println!("ext4 tier1: dry-run");
        println!("ext4 tier1: run-id={}", self.run_id);
        println!(
            "ext4 tier1: actions {}",
            self.planned_actions().join(" -> ")
        );
        println!(
            "ext4 tier1: capability-ledger {} sha256 {}",
            self.authorities.capability.path.display(),
            self.authorities.capability.sha256
        );
        println!(
            "ext4 tier1: xfstests-selection {} sha256 {}",
            self.authorities.selection.file.path.display(),
            self.authorities.selection.sha256()
        );
        println!(
            "ext4 tier1: xfstests source {} revision {} check-sha256 {}",
            self.authorities.selection.source_lock.path.display(),
            self.authorities.selection.source_lock.revision,
            self.authorities.selection.source_lock.check_sha256
        );
        println!(
            "ext4 tier1: crash-cuts {} sha256 {}",
            self.authorities.crash_cuts.file.path.display(),
            self.authorities.crash_cuts.sha256()
        );
        if let Some(campaign) = &self.authorities.crash_cuts.campaign {
            println!(
                "ext4 tier1: crash campaign workload={} sha256 {} replay={} sha256 {} kill-policy={} e2fsck-mode={}",
                campaign.workload_script.display(),
                campaign.workload_script_sha256,
                campaign.replay_script.display(),
                campaign.replay_script_sha256,
                campaign.kill_policy,
                campaign.e2fsck_mode
            );
        }
        println!(
            "ext4 tier1: shell-scenario {} sha256 {}",
            self.authorities.shell_scenario.path.display(),
            self.authorities.shell_scenario.sha256
        );
        println!(
            "ext4 tier1: pinned xfstests cases {}",
            self.authorities.selection.case_count
        );
        println!(
            "ext4 tier1: deterministic crash cuts {}",
            self.authorities.crash_cuts.expanded_cut_count
        );
    }

    fn planned_actions(&self) -> Vec<String> {
        vec![
            "run G0 ext4 ownership and durability lints".into(),
            "build candidate".into(),
            "build busybox ext4 base image".into(),
            "create fresh TEST/SCRATCH/WORKLOAD images".into(),
            "run guest matrix".into(),
            "execute deterministic crash cuts".into(),
            "replay and collect immutable image copies".into(),
            "run e2fsck -fn on every immutable image".into(),
            "run pinned xfstests selection".into(),
            "write immutable acceptance receipt".into(),
        ]
    }
}

fn run_g0_lints(root: &Path) -> Result<()> {
    for rule in G0_EXT4_LINTS {
        run_cmd_owned_in(
            root,
            "cargo",
            &[
                "xtask".into(),
                "lint".into(),
                "invariants".into(),
                (*rule).into(),
            ],
        )?;
    }
    Ok(())
}

fn parse_tier1_args(root: &Path, args: &[String]) -> Result<Tier1Invocation> {
    let args = match args.first().map(String::as_str) {
        Some("tier1") => &args[1..],
        _ => args,
    };
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--dry-run" => idx += 1,
            "--run-id" => {
                let Some(value) = args.get(idx + 1) else {
                    return Err("option --run-id needs a value".into());
                };
                if value.starts_with("--") {
                    return Err("option --run-id needs a value".into());
                }
                idx += 2;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown ext4 tier1 flag '{other}'"));
            }
            other => {
                return Err(format!("unexpected positional argument '{other}'"));
            }
        }
    }
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let run_id = optional_option_value(args, "--run-id").unwrap_or_else(|| "tier1-dry-run".into());
    let authorities = Tier1Authorities::load(root)?;
    Ok(Tier1Invocation {
        run_id,
        dry_run,
        authorities,
    })
}

#[derive(Debug)]
struct Tier1Authorities {
    capability: AuthorityFile,
    selection: XfstestsSelection,
    crash_cuts: CrashCutCatalog,
    shell_scenario: AuthorityFile,
}

impl Tier1Authorities {
    fn load(root: &Path) -> Result<Self> {
        Ok(Self {
            capability: AuthorityFile::load(root.join("tools/ext4/tier1/capability-ledger.json"))?,
            selection: XfstestsSelection::load(
                root.join("tools/ext4/tier1/xfstests-selection.json"),
            )?,
            crash_cuts: CrashCutCatalog::load_with_root(
                root.join("tools/ext4/tier1/crash-cuts.json"),
                root,
            )?,
            shell_scenario: AuthorityFile::load(root.join("tools/shell-tests/ext4-tier1.scn"))?,
        })
    }

    fn as_input_summary(&self) -> receipt::Tier1AuthorityInputs {
        receipt::authority_input_summary(
            self.capability.sha256.clone(),
            self.crash_cuts.sha256().to_string(),
            self.selection.sha256().to_string(),
            self.shell_scenario.sha256.clone(),
            self.selection.case_count,
            self.crash_cuts.expanded_cut_count,
        )
    }

    fn ensure_live_acceptance_ready(&self) -> Result<()> {
        let mut blockers = Vec::new();
        if let Some(blocker) = self.selection.live_acceptance_blocker() {
            blockers.push(blocker);
        }
        if let Some(blocker) = self.crash_cuts.live_acceptance_blocker() {
            blockers.push(blocker);
        }
        if blockers.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "live ext4 Tier 1 authorities are not acceptance-ready: {}",
                blockers.join("; ")
            ))
        }
    }
}

#[derive(Debug)]
struct AuthorityFile {
    path: PathBuf,
    sha256: String,
}

impl AuthorityFile {
    fn load(path: PathBuf) -> Result<Self> {
        let bytes =
            fs::read(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        Ok(Self {
            path,
            sha256: hex_string(tx_ext4_format::capability::sha256(&bytes)),
        })
    }
}

#[derive(Debug)]
struct XfstestsSelection {
    file: AuthorityFile,
    status: String,
    source_lock: XfstestsSourceLock,
    case_count: usize,
    cases: Vec<String>,
}

#[derive(Debug, Clone)]
struct XfstestsSourceLock {
    path: PathBuf,
    revision: String,
    check_sha256: String,
}

impl XfstestsSelection {
    fn load(path: PathBuf) -> Result<Self> {
        let path_display = path.display().to_string();
        let value: serde_json::Value = read_json(&path)?;
        let schema = value
            .get("schema")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                format!(
                    "{}: missing schema tx.ext4.xfstests_selection_ledger.v1",
                    path.display()
                )
            })?;
        if schema != "tx.ext4.xfstests_selection_ledger.v1" {
            return Err(format!(
                "{}: expected schema tx.ext4.xfstests_selection_ledger.v1, found {schema}",
                path.display()
            ));
        }
        let tier = value
            .get("tier")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("{}: missing tier", path.display()))?;
        if tier != "tier1" {
            return Err(format!("{}: expected tier1, found {tier}", path.display()));
        }
        let status = value
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unspecified")
            .to_string();
        let source_lock = XfstestsSourceLock::load(&value, &path)?;
        let cases = value
            .get("selected")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{}: missing selected case list", path.display()))?;
        if cases.is_empty() {
            return Err(format!("{}: selected case list is empty", path.display()));
        }
        Ok(Self {
            file: AuthorityFile::load(path)?,
            status,
            source_lock,
            case_count: cases.len(),
            cases: cases
                .iter()
                .map(|case| {
                    case.get("case_id")
                        .and_then(|value| value.as_str())
                        .or_else(|| case.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            format!("{}: selected case entry missing case_id", path_display)
                        })
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }

    fn sha256(&self) -> &str {
        &self.file.sha256
    }

    fn live_acceptance_blocker(&self) -> Option<String> {
        if self.status == "acceptance-ready" {
            None
        } else {
            Some(format!(
                "xfstests selection status is `{}`; expected `acceptance-ready`",
                self.status
            ))
        }
    }
}

impl XfstestsSourceLock {
    fn load(value: &serde_json::Value, path: &Path) -> Result<Self> {
        let source_lock = value
            .get("source_lock")
            .and_then(|value| value.as_object())
            .ok_or_else(|| format!("{}: missing source_lock", path.display()))?;
        let source_path = source_lock
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("{}: missing source_lock.path", path.display()))?;
        let source_path = PathBuf::from(source_path);
        if source_path.is_absolute()
            || source_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!(
                "{}: source_lock.path must be repository-relative without `..`",
                path.display()
            ));
        }
        let revision = source_lock
            .get("revision")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("{}: missing source_lock.revision", path.display()))?;
        if revision.len() != 40 || !revision.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(format!(
                "{}: source_lock.revision must be a 40-byte hex commit",
                path.display()
            ));
        }
        let check_sha256 = source_lock
            .get("check_sha256")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("{}: missing source_lock.check_sha256", path.display()))?;
        if check_sha256.len() != 64 || !check_sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(format!(
                "{}: source_lock.check_sha256 must be a 64-byte hex digest",
                path.display()
            ));
        }
        Ok(Self {
            path: source_path,
            revision: revision.to_ascii_lowercase(),
            check_sha256: check_sha256.to_ascii_lowercase(),
        })
    }

    fn root_path(&self, root: &Path) -> PathBuf {
        root.join(&self.path)
    }
}

#[derive(Debug)]
struct CrashCutCatalog {
    file: AuthorityFile,
    status: String,
    expanded_cut_count: usize,
    families: Vec<CrashCutFamily>,
    campaign: Option<CrashCutCampaignPlan>,
}

#[derive(Debug, Clone)]
struct CrashCutFamily {
    id: String,
    phase_marker: Option<String>,
}

#[derive(Debug)]
struct CrashCutCampaignPlan {
    workload_script: PathBuf,
    workload_script_sha256: String,
    replay_script: PathBuf,
    replay_script_sha256: String,
    kill_policy: String,
    e2fsck_mode: String,
}

impl CrashCutCatalog {
    #[allow(dead_code)]
    fn load(path: PathBuf) -> Result<Self> {
        Self::load_inner(path, None)
    }

    fn load_with_root(path: PathBuf, root: &Path) -> Result<Self> {
        Self::load_inner(path, Some(root))
    }

    fn load_inner(path: PathBuf, root: Option<&Path>) -> Result<Self> {
        let value: serde_json::Value = read_json(&path)?;
        let schema = value
            .get("schema")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                format!(
                    "{}: missing schema tx.ext4.crash_cut_catalog.v1",
                    path.display()
                )
            })?;
        if schema != "tx.ext4.crash_cut_catalog.v1" {
            return Err(format!(
                "{}: expected schema tx.ext4.crash_cut_catalog.v1, found {schema}",
                path.display()
            ));
        }
        let status = value
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unspecified")
            .to_string();
        let expanded_cut_count = value
            .get("expanded_cut_count")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| format!("{}: missing expanded_cut_count", path.display()))?;
        if expanded_cut_count != 1000 {
            return Err(format!(
                "{}: expected expanded_cut_count 1000, found {expanded_cut_count}",
                path.display()
            ));
        }
        let families = value
            .get("families")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{}: missing families", path.display()))?;
        let expected = [
            "D0", "D1", "D2", "D3", "D4", "D5", "D6", "D7", "D8", "D9", "D10", "D11", "D12",
        ];
        for family in expected {
            let present = families.iter().any(|entry| {
                entry
                    .get("id")
                    .and_then(|value| value.as_str())
                    .is_some_and(|id| id == family)
            });
            if !present {
                return Err(format!(
                    "{}: missing stable crash family {family}",
                    path.display()
                ));
            }
        }
        let families = families
            .iter()
            .map(|entry| CrashCutFamily::parse(entry, &path))
            .collect::<Result<Vec<_>>>()?;
        let campaign = CrashCutCampaignPlan::load_optional(&value, &path, root)?;
        if campaign.is_some() {
            validate_family_phase_markers(&families, &path)?;
        }
        Ok(Self {
            file: AuthorityFile::load(path)?,
            status,
            expanded_cut_count: expanded_cut_count as usize,
            families,
            campaign,
        })
    }

    fn sha256(&self) -> &str {
        &self.file.sha256
    }

    fn live_acceptance_blocker(&self) -> Option<String> {
        if self.status != "acceptance-ready" {
            return Some(format!(
                "crash-cut catalog status is `{}`; expected `acceptance-ready`",
                self.status
            ));
        }
        if self.campaign.is_none() {
            return Some("crash-cut catalog is acceptance-ready but missing campaign plan".into());
        }
        None
    }
}

impl CrashCutFamily {
    fn parse(value: &serde_json::Value, path: &Path) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: crash family entries must be objects", path.display()))?;
        let id = required_object_string(object, "id", path)?;
        let phase_marker = object
            .get("phase_marker")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        Ok(Self { id, phase_marker })
    }
}

fn validate_family_phase_markers(families: &[CrashCutFamily], path: &Path) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for family in families {
        let marker = family.phase_marker.as_deref().ok_or_else(|| {
            format!(
                "{}: crash family {} missing phase_marker for deterministic-phase-marker-v1",
                path.display(),
                family.id
            )
        })?;
        if !seen.insert(marker.to_string()) {
            return Err(format!(
                "{}: duplicate crash phase_marker {marker}",
                path.display()
            ));
        }
    }
    Ok(())
}

impl CrashCutCampaignPlan {
    fn load_optional(
        value: &serde_json::Value,
        path: &Path,
        root: Option<&Path>,
    ) -> Result<Option<Self>> {
        let Some(campaign) = value.get("campaign") else {
            return Ok(None);
        };
        let campaign = campaign
            .as_object()
            .ok_or_else(|| format!("{}: campaign must be an object", path.display()))?;
        let workload_script = required_repo_relative_path(campaign, "workload_script", path)?;
        let replay_script = required_repo_relative_path(campaign, "replay_script", path)?;
        let kill_policy = required_string(campaign, "kill_policy", path)?;
        if kill_policy != "deterministic-phase-marker-v1" {
            return Err(format!(
                "{}: campaign.kill_policy must be deterministic-phase-marker-v1",
                path.display()
            ));
        }
        let e2fsck_mode = required_string(campaign, "e2fsck_mode", path)?;
        if e2fsck_mode != "immutable-copy" {
            return Err(format!(
                "{}: campaign.e2fsck_mode must be immutable-copy",
                path.display()
            ));
        }
        let workload_script_sha256 =
            required_existing_campaign_artifact_sha256(root, &workload_script, "workload_script")?;
        let replay_script_sha256 =
            required_existing_campaign_artifact_sha256(root, &replay_script, "replay_script")?;
        Ok(Some(Self {
            workload_script,
            workload_script_sha256,
            replay_script,
            replay_script_sha256,
            kill_policy,
            e2fsck_mode,
        }))
    }
}

fn required_existing_campaign_artifact_sha256(
    root: Option<&Path>,
    relative_path: &Path,
    key: &str,
) -> Result<String> {
    let Some(root) = root else {
        return Ok("0".repeat(64));
    };
    let artifact = root.join(relative_path);
    if !artifact.is_file() {
        return Err(format!(
            "missing campaign.{key} artifact {}",
            artifact.display()
        ));
    }
    sha256_file(&artifact)
}

fn required_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{}: missing campaign.{key}", path.display()))
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

fn required_repo_relative_path(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<PathBuf> {
    let value = required_string(object, key, path)?;
    let value = PathBuf::from(value);
    if value.is_absolute()
        || value
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "{}: campaign.{key} must be repository-relative without `..`",
            path.display()
        ));
    }
    Ok(value)
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn hex_string(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn run_capture(cwd: &Path, program: &str, args: &[String]) -> Result<(i32, String)> {
    println!("$ {} {}", program, shell_join(args));
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.code().unwrap_or(-1), combined))
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    Ok(hex_string(tx_ext4_format::capability::sha256(&bytes)))
}

fn git_head(root: &Path) -> Result<String> {
    let (code, output) = run_capture(root, "git", &["rev-parse".into(), "HEAD".into()])?;
    if code != 0 {
        return Err(format!("git rev-parse HEAD exited with {code}"));
    }
    Ok(output.trim().to_string())
}

fn ensure_xfstests_root(
    root: &Path,
    run: &run_workspace::RunWorkspace,
    source_lock: &XfstestsSourceLock,
) -> Result<PathBuf> {
    if !command_exists("git") {
        return Err("git is required to materialize the pinned xfstests source".into());
    }
    let preferred = source_lock.root_path(root);
    if preferred.join("check").is_file() {
        verify_xfstests_source_lock(&preferred, source_lock)?;
        return Ok(preferred);
    }

    let clone_dir = run.working_dir().join("xfstests-source");
    if clone_dir.join("check").is_file() {
        verify_xfstests_source_lock(&clone_dir, source_lock)?;
        return Ok(clone_dir);
    }

    let source_url = "https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git";
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).map_err(|err| {
            format!(
                "failed to remove stale xfstests clone {}: {err}",
                clone_dir.display()
            )
        })?;
    }
    run_cmd_owned_in(
        run.working_dir(),
        "git",
        &[
            "clone".into(),
            "--no-checkout".into(),
            source_url.into(),
            clone_dir.display().to_string(),
        ],
    )?;
    run_cmd_owned_in(
        &clone_dir,
        "git",
        &[
            "checkout".into(),
            "--detach".into(),
            source_lock.revision.clone(),
        ],
    )?;
    verify_xfstests_source_lock(&clone_dir, source_lock)?;
    Ok(clone_dir)
}

fn verify_xfstests_source_lock(
    xfstests_root: &Path,
    source_lock: &XfstestsSourceLock,
) -> Result<()> {
    let (code, output) = run_capture(xfstests_root, "git", &["rev-parse".into(), "HEAD".into()])?;
    if code != 0 {
        return Err(format!(
            "failed to read xfstests revision at {}",
            xfstests_root.display()
        ));
    }
    let revision = output.trim().to_ascii_lowercase();
    if revision != source_lock.revision {
        return Err(format!(
            "xfstests revision mismatch at {}: expected {}, found {}",
            xfstests_root.display(),
            source_lock.revision,
            revision
        ));
    }
    let check = xfstests_root.join("check");
    let check_sha256 = sha256_file(&check)?;
    if check_sha256 != source_lock.check_sha256 {
        return Err(format!(
            "xfstests check sha256 mismatch at {}: expected {}, found {}",
            check.display(),
            source_lock.check_sha256,
            check_sha256
        ));
    }
    Ok(())
}

fn run_xfstests_selection(
    root: &Path,
    run: &mut run_workspace::RunWorkspace,
    authorities: &Tier1Authorities,
) -> Result<receipt::XfstestsSummary> {
    let xfstests_root = ensure_xfstests_root(root, run, &authorities.selection.source_lock)?;
    let check = xfstests_root.join("check");
    if !check.is_file() {
        return Err(format!("missing xfstests check script {}", check.display()));
    }

    let tests = authorities.selection.cases.clone();
    let mut args = Vec::with_capacity(tests.len() + 1);
    args.push("--help".into());
    let (help_code, help_output) = run_capture(&xfstests_root, "./check", &args)?;
    if help_code != 0 && help_output.is_empty() {
        return Err(format!(
            "xfstests check helper at {} did not execute successfully",
            xfstests_root.display()
        ));
    }

    let case_args = tests.clone();
    let (code, output) = run_capture(&xfstests_root, "./check", &case_args)?;
    let log_path = run.working_dir().join("xfstests.log");
    fs::write(&log_path, &output)
        .map_err(|err| format!("failed to write {}: {err}", log_path.display()))?;
    run.record_artifact("xfstests-log", log_path)?;
    if code != 0 {
        return Err(format!(
            "xfstests selection exited with {code}\n{}",
            output.trim_end()
        ));
    }

    parse_xfstests_summary(&output, tests.len())
}

fn parse_xfstests_summary(output: &str, expected_cases: usize) -> Result<receipt::XfstestsSummary> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Not run:") {
            return Err(format!("xfstests reported not-run cases: {trimmed}"));
        }
        if trimmed.starts_with("Failures:") || trimmed.starts_with("Failed ") {
            return Err(format!("xfstests reported failures: {trimmed}"));
        }
    }

    let mut passed_all = None;
    for line in output.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Passed all ") else {
            continue;
        };
        let Some(count) = rest.split_whitespace().next() else {
            continue;
        };
        let Ok(count) = count.parse::<usize>() else {
            continue;
        };
        passed_all = Some(count);
    }

    let Some(passed) = passed_all else {
        return Err("xfstests output missing `Passed all N tests` summary".into());
    };
    if passed != expected_cases {
        return Err(format!(
            "xfstests passed count mismatch: expected {expected_cases}, output reported {passed}"
        ));
    }

    Ok(receipt::XfstestsSummary {
        skipped: 0,
        not_run: 0,
        passed,
        failed: 0,
    })
}
