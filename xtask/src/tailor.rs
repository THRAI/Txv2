use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{relative, resolve_path, shell_escape, shell_join};
use crate::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LtpLibrary {
    Musl,
    Glibc,
}

impl LtpLibrary {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "musl" => Ok(Self::Musl),
            "glibc" => Ok(Self::Glibc),
            other => Err(format!(
                "unknown LTP library '{other}', expected musl or glibc"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Musl => "musl",
            Self::Glibc => "glibc",
        }
    }

    fn suite(self) -> &'static str {
        match self {
            Self::Musl => "ltp-musl",
            Self::Glibc => "ltp-glibc",
        }
    }

    fn default_size_mb(self) -> u32 {
        match self {
            Self::Musl => 256,
            Self::Glibc => 512,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TailorArch {
    Rv64,
    La64,
}

impl TailorArch {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "rv64" => Ok(Self::Rv64),
            "la64" => Ok(Self::La64),
            other => Err(format!("unknown arch '{other}', expected rv64 or la64")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rv64 => "rv64",
            Self::La64 => "la64",
        }
    }

    fn image_name(self) -> &'static str {
        match self {
            Self::Rv64 => "sdcard-rv.img",
            Self::La64 => "sdcard-la.img",
        }
    }

    fn qemu_target(self) -> &'static str {
        match self {
            Self::Rv64 => "rv64-qemu",
            Self::La64 => "la64-qemu",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct LtpPlan {
    suite: String,
    tests: String,
    source: PathBuf,
    output: PathBuf,
    size_mb: u32,
    list_cases: bool,
    arch: TailorArch,
}

pub(crate) fn tailor(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("tailor command needs a subcommand, expected ltp".into());
    };
    match kind.as_str() {
        "ltp" => tailor_ltp(root, &args[1..]),
        other => Err(format!("unknown tailor subcommand '{other}', expected ltp")),
    }
}

fn tailor_ltp(root: &Path, args: &[String]) -> Result<()> {
    let plan = parse_ltp_plan(root, args)?;
    let script = root.join("tools").join("build-slim-sdcard.py");
    if !script.exists() {
        return Err(format!(
            "missing {}; this command requires the Python helper script",
            script.display()
        ));
    }

    let mut cmd_args = vec![
        script.display().to_string(),
        "--source".into(),
        plan.source.display().to_string(),
    ];

    if plan.list_cases {
        cmd_args.push("--list-cases".into());
        cmd_args.push(plan.suite.clone());
    } else {
        if let Some(parent) = plan.output.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        cmd_args.push("--suite".into());
        cmd_args.push(plan.suite.clone());
        cmd_args.push("--ltp-cases".into());
        cmd_args.push(plan.tests.clone());
        cmd_args.push("--output".into());
        cmd_args.push(plan.output.display().to_string());
        cmd_args.push("--size-mb".into());
        cmd_args.push(plan.size_mb.to_string());
    }

    println!("$ python3 {}", shell_join(&cmd_args));
    let status = Command::new("python3")
        .args(&cmd_args)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run build-slim-sdcard.py: {err}"))?;
    if !status.success() {
        return Err(format!("build-slim-sdcard.py exited with {status}"));
    }

    if !plan.list_cases {
        let data_dir = plan
            .output
            .parent()
            .ok_or_else(|| format!("output path {} has no parent", plan.output.display()))?;
        println!("tailored LTP image: {}", plan.output.display());
        println!(
            "next: cargo xtask oscomp qemu --target {} --data {} --submit target/oscomp/submit --boot-suite {}",
            plan.arch.qemu_target(),
            shell_escape(&relative(root, data_dir)),
            plan.suite
        );
    }

    Ok(())
}

fn parse_ltp_plan(root: &Path, args: &[String]) -> Result<LtpPlan> {
    if args.is_empty() {
        return Err("missing subcommand options for tailor ltp".into());
    }

    let mut library = None;
    let mut arch = None;
    let mut tests = None;
    let mut data = None;
    let mut source = None;
    let mut output = None;
    let mut size_mb = None;
    let mut list_cases = false;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--library" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --library needs a value".to_string())?;
                library = Some(LtpLibrary::parse(value)?);
            }
            "--arch" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --arch needs a value".to_string())?;
                arch = Some(TailorArch::parse(value)?);
            }
            "--tests" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --tests needs a value".to_string())?;
                if value.trim().is_empty() {
                    return Err("option --tests cannot be empty".into());
                }
                tests = Some(value.clone());
            }
            "--data" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --data needs a value".to_string())?;
                data = Some(resolve_path(root, PathBuf::from(value)));
            }
            "--source" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --source needs a value".to_string())?;
                source = Some(resolve_path(root, PathBuf::from(value)));
            }
            "--output" | "-o" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| format!("option {} needs a value", args[idx - 1]))?;
                output = Some(resolve_path(root, PathBuf::from(value)));
            }
            "--size-mb" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "option --size-mb needs a value".to_string())?;
                size_mb = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --size-mb value '{value}'"))?,
                );
            }
            "--list-cases" => {
                list_cases = true;
            }
            "-h" | "--help" => {
                return Err(ltp_usage());
            }
            other => return Err(format!("unknown tailor ltp option '{other}'")),
        }
        idx += 1;
    }

    let library = library.ok_or_else(|| "missing required option --library".to_string())?;
    let arch = arch.ok_or_else(|| "missing required option --arch".to_string())?;
    let data = data.unwrap_or_else(|| root.join("target/oscomp/testdata"));
    let source = source.unwrap_or_else(|| default_source(&data, library, arch));
    let output = output.unwrap_or_else(|| {
        root.join("target")
            .join("oscomp")
            .join("tailor")
            .join(format!("ltp-{}-{}", library.name(), arch.name()))
            .join(arch.image_name())
    });

    let tests = if list_cases {
        tests.unwrap_or_default()
    } else {
        tests.ok_or_else(|| "--tests is required unless --list-cases is used".to_string())?
    };

    Ok(LtpPlan {
        suite: library.suite().to_string(),
        tests,
        source,
        output,
        size_mb: size_mb.unwrap_or_else(|| library.default_size_mb()),
        list_cases,
        arch,
    })
}

fn default_source(data: &Path, library: LtpLibrary, arch: TailorArch) -> PathBuf {
    if library == LtpLibrary::Musl && arch == TailorArch::Rv64 {
        let case_image = data.join("sdcard-ltp-cases-rv.img");
        if case_image.exists() {
            return case_image;
        }
    }
    data.join(arch.image_name())
}

fn ltp_usage() -> String {
    "usage: cargo xtask tailor ltp --library musl|glibc --arch rv64|la64 --tests CASE[,CASE...] [--data DIR] [--source IMG] [--output IMG] [--size-mb N] [--list-cases]".into()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn ltp_plan_accepts_musl_rv64_comma_tests() {
        let root = unique_test_root("musl-rv64");
        fs::create_dir_all(root.join("target/oscomp/testdata")).unwrap();
        fs::write(
            root.join("target/oscomp/testdata/sdcard-ltp-cases-rv.img"),
            b"",
        )
        .unwrap();

        let plan = parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "musl".into(),
                "--arch".into(),
                "rv64".into(),
                "--tests".into(),
                "access01,access02".into(),
            ],
        )
        .unwrap();

        assert_eq!(plan.suite, "ltp-musl");
        assert_eq!(plan.tests, "access01,access02");
        assert_eq!(plan.size_mb, 256);
        assert_eq!(
            plan.source,
            root.join("target/oscomp/testdata/sdcard-ltp-cases-rv.img")
        );
        assert_eq!(
            plan.output,
            root.join("target/oscomp/tailor/ltp-musl-rv64/sdcard-rv.img")
        );
    }

    #[test]
    fn ltp_plan_falls_back_to_rv64_default_source() {
        let root = unique_test_root("rv64-fallback");
        fs::create_dir_all(root.join("target/oscomp/testdata")).unwrap();

        let plan = parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "musl".into(),
                "--arch".into(),
                "rv64".into(),
                "--tests".into(),
                "access01".into(),
            ],
        )
        .unwrap();

        assert_eq!(
            plan.source,
            root.join("target/oscomp/testdata/sdcard-rv.img")
        );
    }

    #[test]
    fn ltp_plan_uses_la64_source_and_glibc_defaults() {
        let root = unique_test_root("glibc-la64");
        fs::create_dir_all(root.join("target/oscomp/testdata")).unwrap();

        let plan = parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "glibc".into(),
                "--arch".into(),
                "la64".into(),
                "--tests".into(),
                "open01".into(),
            ],
        )
        .unwrap();

        assert_eq!(plan.suite, "ltp-glibc");
        assert_eq!(plan.size_mb, 512);
        assert_eq!(
            plan.source,
            root.join("target/oscomp/testdata/sdcard-la.img")
        );
        assert_eq!(
            plan.output,
            root.join("target/oscomp/tailor/ltp-glibc-la64/sdcard-la.img")
        );
    }

    #[test]
    fn ltp_plan_rejects_invalid_inputs() {
        let root = unique_test_root("invalid");

        assert!(parse_ltp_plan(&root, &[])
            .unwrap_err()
            .contains("missing subcommand"));
        assert!(parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "uclibc".into(),
                "--arch".into(),
                "rv64".into(),
                "--tests".into(),
                "access01".into(),
            ],
        )
        .unwrap_err()
        .contains("unknown LTP library"));
        assert!(parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "musl".into(),
                "--arch".into(),
                "x86".into(),
                "--tests".into(),
                "access01".into(),
            ],
        )
        .unwrap_err()
        .contains("unknown arch"));
        assert!(parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "musl".into(),
                "--arch".into(),
                "rv64".into(),
            ],
        )
        .unwrap_err()
        .contains("--tests is required"));
    }

    #[test]
    fn ltp_plan_list_cases_does_not_require_tests() {
        let root = unique_test_root("list-cases");

        let plan = parse_ltp_plan(
            &root,
            &[
                "--library".into(),
                "musl".into(),
                "--arch".into(),
                "rv64".into(),
                "--list-cases".into(),
            ],
        )
        .unwrap();

        assert!(plan.list_cases);
        assert!(plan.tests.is_empty());
        assert_eq!(plan.suite, "ltp-musl");
    }

    fn unique_test_root(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("tx-tailor-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
