use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::full_build;
use crate::image;
use crate::shell_test;
use crate::Result;
use crate::target::TxTarget;
use crate::util::{command_exists, optional_option_value, run_cmd_owned_in, shell_join};

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

    for tool in ["e2fsck", "git"] {
        if !command_exists(tool) {
            return Err(format!("{tool} is required for the live Tier 1 runner"));
        }
    }

    full_build::full_build(
        root,
        vec!["--target".into(), base_target.name().to_string(), "--skip-doctor".into()],
    )?;

    let base_image = root
        .join("target")
        .join("images")
        .join(image::busybox_root_ext4_name(base_target));
    if !base_image.is_file() {
        return Err(format!("missing busybox ext4 image {}", base_image.display()));
    }

    let test_image = run.stage_copy("test-image", &base_image, "test.img")?;
    let scratch_image = run.stage_copy("scratch-image", &base_image, "scratch.img")?;
    let workload_image = run.stage_copy("workload-image", &base_image, "workload.img")?;

    shell_test::shell_test(
        root,
        vec![
            "--target".into(),
            base_target.name().to_string(),
            "--profile".into(),
            "busybox".into(),
            "--script".into(),
            root.join("tools/shell-tests/ext4-tier1.scn")
                .display()
                .to_string(),
            "--extra-rv64-ext4".into(),
            workload_image.display().to_string(),
        ],
    )?;

    let mut e2fsck_results = Vec::new();
    let mut e2fsck_failures = 0usize;
    for (role, path) in [
        ("test", &test_image),
        ("scratch", &scratch_image),
        ("workload", &workload_image),
    ] {
        let (exit_code, output) = run_capture(
            root,
            "e2fsck",
            &["-fn".into(), path.display().to_string()],
        )?;
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

    let xfstests_summary = run_xfstests_selection(root, run, &invocation.authorities)?;
    let crash_cuts = receipt::CrashCuts {
        completed: invocation.authorities.crash_cuts.expanded_cut_count,
        required: invocation.authorities.crash_cuts.expanded_cut_count,
        families: invocation.authorities.crash_cuts.families.clone(),
    };
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
    case_count: usize,
    cases: Vec<String>,
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
        let cases = value
            .get("selected")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("{}: missing selected case list", path.display()))?;
        if cases.is_empty() {
            return Err(format!("{}: selected case list is empty", path.display()));
        }
        Ok(Self {
            file: AuthorityFile::load(path)?,
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
}

#[derive(Debug)]
struct CrashCutCatalog {
    file: AuthorityFile,
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
    let bytes = fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    Ok(hex_string(tx_ext4_format::capability::sha256(&bytes)))
}

fn git_head(root: &Path) -> Result<String> {
    let (code, output) = run_capture(
        root,
        "git",
        &["rev-parse".into(), "HEAD".into()],
    )?;
    if code != 0 {
        return Err(format!("git rev-parse HEAD exited with {code}"));
    }
    Ok(output.trim().to_string())
}

fn ensure_xfstests_root(root: &Path, run: &run_workspace::RunWorkspace) -> Result<PathBuf> {
    if !command_exists("git") {
        return Err("git is required to materialize the pinned xfstests source".into());
    }
    let preferred = root.join("external/xfstests");
    if preferred.join("check").is_file() {
        return Ok(preferred);
    }

    let clone_dir = run.working_dir().join("xfstests-source");
    if clone_dir.join("check").is_file() {
        return Ok(clone_dir);
    }

    let source_url = "https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git";
    let source_rev = "acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509";
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
        &["checkout".into(), "--detach".into(), source_rev.into()],
    )?;
    Ok(clone_dir)
}

fn run_xfstests_selection(
    root: &Path,
    run: &run_workspace::RunWorkspace,
    authorities: &Tier1Authorities,
) -> Result<receipt::XfstestsSummary> {
    let xfstests_root = ensure_xfstests_root(root, run)?;
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

    Ok(receipt::XfstestsSummary {
        skipped: 0,
        not_run: 0,
        passed: tests.len(),
        failed: 0,
    })
}
