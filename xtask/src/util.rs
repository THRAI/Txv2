use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Result;

pub(crate) fn resolve_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

pub(crate) fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).map_err(|err| err.to_string())?;
    for entry in fs::read_dir(src).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|err| err.to_string())?.is_dir() {
            if to.exists() {
                fs::remove_dir_all(&to).map_err(|err| err.to_string())?;
            }
            copy_dir_contents(&from, &to)?;
        } else {
            if to.exists() {
                fs::remove_file(&to).map_err(|err| err.to_string())?;
            }
            fs::copy(&from, &to).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn check_version(
    program: &str,
    args: &[&str],
    required: bool,
    missing: &mut Vec<String>,
) -> Result<()> {
    if !command_exists(program) {
        if required {
            println!("missing: {program}");
            missing.push(program.to_string());
        } else {
            println!("warn: optional {program} not found");
        }
        return Ok(());
    }

    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next().unwrap_or("ok");
    println!("ok: {first}");
    Ok(())
}

pub(crate) fn option_value(args: &[String], name: &str) -> Result<String> {
    let Some(idx) = args.iter().position(|arg| arg == name) else {
        return Err(format!("missing required option {name}"));
    };
    if idx + 1 >= args.len() {
        return Err(format!("option {name} needs a value"));
    }
    Ok(args[idx + 1].clone())
}

pub(crate) fn optional_option_value(args: &[String], name: &str) -> Option<String> {
    let idx = args.iter().position(|arg| arg == name)?;
    args.get(idx + 1).cloned()
}

pub(crate) fn run_cmd(root: &Path, program: &str, args: &[&str]) -> Result<()> {
    println!("$ {} {}", program, args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

pub(crate) fn run_cmd_owned(root: &Path, program: &str, args: &[String]) -> Result<()> {
    println!("$ {} {}", program, shell_join(args));
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

pub(crate) fn run_cmd_owned_in(cwd: &Path, program: &str, args: &[String]) -> Result<()> {
    println!("$ {} {}", program, shell_join(args));
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

pub(crate) fn run_shell(root: &Path, script: &str) -> Result<()> {
    println!("$ sh -c {}", script);
    let status = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| format!("failed to run shell: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("shell command exited with {status}"))
    }
}

pub(crate) fn command_display(program: &str, args: &[&str]) -> String {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().map(|arg| (*arg).to_string()));
    shell_join(&parts)
}

pub(crate) struct CargoOutcome {
    pub ok: bool,
    /// Key summary line extracted from output (e.g. "test result: ok. 233 passed …").
    pub summary: String,
    /// Full combined stderr+stdout, always captured.
    pub output: String,
}

/// Run `cargo` with `args`, capture all output, and extract the key summary line.
/// Nothing is printed; the caller decides how to display the result.
pub(crate) fn compact_cargo(root: &Path, args: &[&str]) -> CargoOutcome {
    let result = Command::new("cargo").args(args).current_dir(root).output();

    match result {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stderr).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stdout));
            let ok = out.status.success();
            let summary = cargo_summary_line(&combined);
            CargoOutcome {
                ok,
                summary,
                output: combined,
            }
        }
        Err(e) => CargoOutcome {
            ok: false,
            summary: String::new(),
            output: format!("failed to start cargo: {e}"),
        },
    }
}

fn cargo_summary_line(output: &str) -> String {
    for line in output.lines().rev() {
        let t = line.trim();
        if t.starts_with("test result:") {
            return t.to_string();
        }
    }
    for line in output.lines().rev() {
        let t = line.trim();
        if t.starts_with("Finished") {
            return t.to_string();
        }
    }
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("ok")
        .trim()
        .to_string()
}

pub(crate) fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

pub(crate) fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_escape(program)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub(crate) fn collect_files(root: &Path, extensions: &[&str]) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, root, extensions, &mut out)?;
    Ok(out)
}

fn collect_files_inner(
    root: &Path,
    dir: &Path,
    extensions: &[&str],
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = relative(root, &path).replace('\\', "/");
        if entry.file_type()?.is_dir() {
            if matches!(rel.as_str(), ".git" | "target") || rel.ends_with("/target") {
                continue;
            }
            if rel.starts_with(".claude/worktrees/") {
                continue;
            }
            collect_files_inner(root, &path, extensions, out)?;
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| extensions.contains(&ext))
        {
            out.push(path);
        }
    }
    Ok(())
}

pub(crate) fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub(crate) fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-:=,+".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
