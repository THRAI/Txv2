use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Result;
use crate::image;
use crate::shell_test;
use crate::target::TxTarget;
use crate::util::{
    command_exists, command_or_candidates, optional_option_value, run_cmd_owned_in, shell_join,
};

mod crash_campaign;
mod receipt;
mod receipt_verify;
mod run_workspace;

pub(crate) use crash_campaign::execute_crash_cut_campaign;
#[cfg(test)]
pub(crate) use crash_campaign::{
    CrashCutCampaignEvidence, CrashCutOutcome, crash_cut_shell_test_args, parse_fault_job_result,
    run_crash_cut_campaign, write_fault_job_request,
};
pub(crate) use receipt_verify::verify_tier1_receipt;

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
    if let Some(receipt_path) = parse_verify_receipt_args(root, args)? {
        verify_tier1_receipt(&receipt_path)?;
        println!("ext4 tier1: verified {}", receipt_path.display());
        return Ok(());
    }
    let invocation = parse_tier1_args(root, args)?;
    if invocation.dry_run {
        invocation.print_dry_run(root);
        return Ok(());
    }
    if invocation.preflight_live {
        return run_live_preflight(root, &invocation.authorities);
    }
    invocation.authorities.ensure_live_acceptance_ready()?;

    let mut run = run_workspace::RunWorkspace::create(root, &invocation.run_id)?;
    run.record_authority_inputs(&invocation.authorities.as_input_summary())?;
    stage_authority_artifacts(&mut run, &invocation.authorities)?;
    run_live_tier1(root, invocation, &mut run)
}

fn stage_authority_artifacts(
    run: &mut run_workspace::RunWorkspace,
    authorities: &Tier1Authorities,
) -> Result<()> {
    run.stage_copy(
        "authority-capability-ledger",
        &authorities.capability.path,
        "authorities/capability-ledger.json",
    )?;
    run.stage_copy(
        "authority-crash-cut-catalog",
        &authorities.crash_cuts.file.path,
        "authorities/crash-cuts.json",
    )?;
    run.stage_copy(
        "authority-xfstests-selection",
        &authorities.selection.file.path,
        "authorities/xfstests-selection.json",
    )?;
    run.stage_copy(
        "authority-shell-scenario",
        &authorities.shell_scenario.path,
        "authorities/ext4-tier1.scn",
    )?;
    Ok(())
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

    run_g0_lints(root, run)?;

    run_and_record_command(
        root,
        run,
        "candidate-full-build-log",
        "build/full-build.log",
        "cargo",
        &[
            "xtask".to_string(),
            "full-build".to_string(),
            "--target".to_string(),
            base_target.name().to_string(),
            "--skip-doctor".to_string(),
        ],
        "candidate full-build",
    )?;
    run_and_record_command(
        root,
        run,
        "busybox-ext4-image-build-log",
        "build/busybox-ext4-image.log",
        "cargo",
        &[
            "xtask".to_string(),
            "image".to_string(),
            "ext4".to_string(),
            "--profile".to_string(),
            "busybox".to_string(),
            "--target".to_string(),
            base_target.name().to_string(),
        ],
        "busybox ext4 image build",
    )?;

    let base_image_source = root
        .join("target")
        .join("images")
        .join(image::busybox_root_ext4_name(base_target));
    if !base_image_source.is_file() {
        return Err(format!(
            "missing busybox ext4 image {}",
            base_image_source.display()
        ));
    }

    let base_image = run.stage_copy("busybox-base-image", &base_image_source, "base.img")?;
    let test_image = run.stage_copy("test-image", &base_image, "test.img")?;
    let scratch_image = run.stage_copy("scratch-image", &base_image, "scratch.img")?;
    let workload_image = run.stage_copy("workload-image", &base_image, "workload.img")?;

    let scenario = root.join("tools/shell-tests/ext4-tier1.scn");
    let guest_matrix_serial = run.working_dir().join("guest-matrix-serial.log");
    let shell_result = shell_test::shell_test(
        root,
        tier1_shell_test_args(
            base_target,
            &scenario,
            &test_image,
            &scratch_image,
            &workload_image,
            &guest_matrix_serial,
        ),
    );
    if guest_matrix_serial.is_file() {
        run.record_artifact("guest-matrix-serial-log", guest_matrix_serial)?;
    }
    shell_result?;

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
    serial_log: &Path,
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
        "--serial-log".into(),
        serial_log.display().to_string(),
    ]
}

fn is_real_sha256(value: &str) -> bool {
    value.len() == 64
        && value.chars().all(|ch| ch.is_ascii_hexdigit())
        && value.chars().any(|ch| ch != '0')
}

#[derive(Debug)]
struct Tier1Invocation {
    run_id: String,
    dry_run: bool,
    preflight_live: bool,
    authorities: Tier1Authorities,
}

impl Tier1Invocation {
    fn print_dry_run(&self, root: &Path) {
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
            "ext4 tier1: xfstests local-source {}",
            self.authorities.selection.source_lock.local_status(root)
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

fn run_live_preflight(root: &Path, authorities: &Tier1Authorities) -> Result<()> {
    let mut blockers = Vec::new();
    println!("ext4 tier1: live-preflight");
    collect_authority_preflight(authorities, &mut blockers);
    collect_tool_preflight(root, authorities, &mut blockers);
    collect_xfstests_preflight(root, authorities, &mut blockers);
    collect_linux_replay_preflight(&mut blockers);
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
            "pinned xfstests source is missing at {}; clone before live preflight can prove execution readiness",
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

fn run_g0_lints(root: &Path, run: &mut run_workspace::RunWorkspace) -> Result<()> {
    for rule in G0_EXT4_LINTS {
        let args = vec![
            "xtask".into(),
            "lint".into(),
            "invariants".into(),
            (*rule).into(),
        ];
        let (code, output) = run_capture(root, "cargo", &args)?;
        let log_path = run.working_dir().join(format!("g0/{rule}.log"));
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        let log = format!("$ cargo {}\nexit_code={code}\n{output}", shell_join(&args));
        fs::write(&log_path, log)
            .map_err(|err| format!("failed to write {}: {err}", log_path.display()))?;
        run.record_artifact(format!("g0-{rule}-log"), log_path)?;
        if code != 0 {
            return Err(format!("G0 lint {rule} exited with {code}"));
        }
    }
    Ok(())
}

fn run_and_record_command(
    cwd: &Path,
    run: &mut run_workspace::RunWorkspace,
    artifact_name: &str,
    log_name: &str,
    program: &str,
    args: &[String],
    failure_label: &str,
) -> Result<()> {
    let (code, output) = run_capture(cwd, program, args)?;
    let log_path = run.working_dir().join(log_name);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let log = format!(
        "$ {program} {}\nexit_code={code}\n{output}",
        shell_join(args)
    );
    fs::write(&log_path, log)
        .map_err(|err| format!("failed to write {}: {err}", log_path.display()))?;
    run.record_artifact(artifact_name, log_path)?;
    if code != 0 {
        return Err(format!("{failure_label} exited with {code}"));
    }
    Ok(())
}

fn parse_tier1_args(root: &Path, args: &[String]) -> Result<Tier1Invocation> {
    let args = strip_optional_tier1(args);
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--dry-run" => idx += 1,
            "--preflight-live" => idx += 1,
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
    let preflight_live = args.iter().any(|arg| arg == "--preflight-live");
    if dry_run && preflight_live {
        return Err("ext4 tier1 accepts only one of --dry-run or --preflight-live".into());
    }
    let run_id = optional_option_value(args, "--run-id").unwrap_or_else(|| "tier1-dry-run".into());
    let authorities = Tier1Authorities::load(root)?;
    Ok(Tier1Invocation {
        run_id,
        dry_run,
        preflight_live,
        authorities,
    })
}

fn strip_optional_tier1(args: &[String]) -> &[String] {
    match args.first().map(String::as_str) {
        Some("tier1") => &args[1..],
        _ => args,
    }
}

fn parse_verify_receipt_args(root: &Path, args: &[String]) -> Result<Option<PathBuf>> {
    let args = strip_optional_tier1(args);
    if !args.iter().any(|arg| arg == "--verify-receipt") {
        return Ok(None);
    }
    if args.len() != 2 || args.first().map(String::as_str) != Some("--verify-receipt") {
        return Err("usage: cargo xtask ext4 tier1 --verify-receipt PATH".into());
    }
    let value = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| "option --verify-receipt needs a value".to_string())?;
    Ok(Some(resolve_repo_path(root, PathBuf::from(value))))
}

fn resolve_repo_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
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

#[derive(Debug)]
struct XfstestsSourceLockEvidence {
    source_root: PathBuf,
    revision: String,
    check_path: PathBuf,
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

    fn local_status(&self, root: &Path) -> String {
        let source_root = self.root_path(root);
        if !source_root.exists() {
            return format!(
                "missing at {}; live runner will clone pinned source before execution",
                source_root.display()
            );
        }
        if !source_root.join("check").is_file() {
            return format!(
                "incomplete at {}; missing check script",
                source_root.display()
            );
        }
        match verify_xfstests_source_lock(&source_root, self) {
            Ok(()) => format!("ready at {}", source_root.display()),
            Err(err) => format!("stale at {}: {err}", source_root.display()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct CrashCutCatalog {
    file: AuthorityFile,
    status: String,
    expanded_cut_count: usize,
    families: Vec<CrashCutFamily>,
    campaign: Option<CrashCutCampaignPlan>,
}

#[derive(Debug, Clone)]
pub(crate) struct CrashCutFamily {
    id: String,
    phase_marker: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CrashCutCampaignPlan {
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
    let evidence = read_xfstests_source_lock_evidence(xfstests_root)?;
    if evidence.revision != source_lock.revision {
        return Err(format!(
            "xfstests revision mismatch at {}: expected {}, found {}",
            xfstests_root.display(),
            source_lock.revision,
            evidence.revision
        ));
    }
    if evidence.check_sha256 != source_lock.check_sha256 {
        return Err(format!(
            "xfstests check sha256 mismatch at {}: expected {}, found {}",
            evidence.check_path.display(),
            source_lock.check_sha256,
            evidence.check_sha256
        ));
    }
    Ok(())
}

fn read_xfstests_source_lock_evidence(xfstests_root: &Path) -> Result<XfstestsSourceLockEvidence> {
    let revision = git_head(xfstests_root)
        .map_err(|_| {
            format!(
                "failed to read xfstests revision at {}",
                xfstests_root.display()
            )
        })?
        .to_ascii_lowercase();
    let check_path = xfstests_root.join("check");
    let check_sha256 = sha256_file(&check_path)?;
    Ok(XfstestsSourceLockEvidence {
        source_root: xfstests_root.to_path_buf(),
        revision,
        check_path,
        check_sha256,
    })
}

fn record_xfstests_source_lock_evidence(
    run: &mut run_workspace::RunWorkspace,
    source_lock: &XfstestsSourceLock,
    xfstests_root: &Path,
) -> Result<()> {
    let evidence = read_xfstests_source_lock_evidence(xfstests_root)?;
    if evidence.revision != source_lock.revision
        || evidence.check_sha256 != source_lock.check_sha256
    {
        verify_xfstests_source_lock(xfstests_root, source_lock)?;
    }
    let evidence_path = run.working_dir().join("xfstests-source-lock-evidence.json");
    let value = serde_json::json!({
        "schema": "tx.ext4.xfstests_source_lock_evidence.v1",
        "source_root": evidence.source_root.display().to_string(),
        "revision": evidence.revision,
        "check_path": evidence.check_path.display().to_string(),
        "check_sha256": evidence.check_sha256,
    });
    fs::write(
        &evidence_path,
        serde_json::to_string_pretty(&value).map_err(|err| {
            format!(
                "failed to encode xfstests source-lock evidence {}: {err}",
                evidence_path.display()
            )
        })? + "\n",
    )
    .map_err(|err| {
        format!(
            "failed to write xfstests source-lock evidence {}: {err}",
            evidence_path.display()
        )
    })?;
    run.record_artifact("xfstests-source-lock-evidence", evidence_path)
}

fn run_xfstests_selection(
    root: &Path,
    run: &mut run_workspace::RunWorkspace,
    authorities: &Tier1Authorities,
) -> Result<receipt::XfstestsSummary> {
    let xfstests_root = ensure_xfstests_root(root, run, &authorities.selection.source_lock)?;
    record_xfstests_source_lock_evidence(run, &authorities.selection.source_lock, &xfstests_root)?;
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
