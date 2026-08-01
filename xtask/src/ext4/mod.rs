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
        tier1_shell_test_args(base_target, &scenario, &scratch_image),
    )?;

    let mut e2fsck_results = Vec::new();
    let mut e2fsck_failures = 0usize;
    for (role, path) in [
        ("test", &test_image),
        ("scratch", &scratch_image),
        ("workload", &workload_image),
    ] {
        let (exit_code, output) =
            run_capture(root, &e2fsck, &["-fn".into(), path.display().to_string()])?;
        if exit_code != 0 {
            e2fsck_failures += 1;
        }
        e2fsck_results.push(receipt::E2fsckImageResult {
            role: role.into(),
            image_sha256: sha256_file(path)?,
            exit_code,
        });
        if !output.trim().is_empty() {
            println!("{output}");
        }
    }

    let crash_cuts = run_crash_cut_campaign(run, &invocation.authorities.crash_cuts)?;
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
        crash_cuts,
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

fn tier1_shell_test_args(target: TxTarget, scenario: &Path, scratch_image: &Path) -> Vec<String> {
    vec![
        "--target".into(),
        target.name().to_string(),
        "--profile".into(),
        "busybox".into(),
        "--script".into(),
        scenario.display().to_string(),
        "--extra-rv64-ext4".into(),
        scratch_image.display().to_string(),
    ]
}

fn run_crash_cut_campaign(
    _run: &run_workspace::RunWorkspace,
    crash_cuts: &CrashCutCatalog,
) -> Result<receipt::CrashCuts> {
    Err(format!(
        "deterministic crash-cut campaign runner is not implemented; refusing to synthesize completed={} across {} families from {}",
        crash_cuts.expanded_cut_count,
        crash_cuts.families.len(),
        crash_cuts.file.path.display()
    ))
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
            crash_cuts: CrashCutCatalog::load(root.join("tools/ext4/tier1/crash-cuts.json"))?,
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
    families: Vec<String>,
}

impl CrashCutCatalog {
    fn load(path: PathBuf) -> Result<Self> {
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
        Ok(Self {
            file: AuthorityFile::load(path)?,
            status,
            expanded_cut_count: expanded_cut_count as usize,
            families: families
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .collect(),
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
                "crash-cut catalog status is `{}`; expected `acceptance-ready`",
                self.status
            ))
        }
    }
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
    run: &run_workspace::RunWorkspace,
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
