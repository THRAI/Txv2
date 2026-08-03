//! `cargo xtask shell-test --target rv64-qemu --script PATH`
//!
//! Drives an interactive `busybox sh` session under QEMU from a
//! line-based script. Captures all serial output for assertion;
//! prints output to the host terminal for visibility.
//!
//! ## Why this and not `expect(1)` / `pexpect`?
//!
//! - txKernel is a bare-metal kernel; we want zero host-side
//!   interpreter dependencies (no python, no tcl).
//! - The script DSL is intentionally tiny so test failures are
//!   debuggable without learning a new language.
//! - Explicit `sleep` directives force timing diversity into tests
//!   to surface heisenbugs. The driver also waits briefly before
//!   each send so prompt echoes do not race the guest's next read.
//!
//! ## Script DSL
//!
//! Line-based, one directive per line. `#` starts a comment. Blank
//! lines are ignored.
//!
//! ```text
//! # comment
//! group NAME               ; start a named group. Directives until
//!                          ; the next `group` (or EOF) belong to it.
//!                          ; Directives before the FIRST `group`
//!                          ; form an implicit `setup` block that
//!                          ; always runs, regardless of `--group`.
//!                          ; NAME is the rest of the line (trimmed);
//!                          ; wrap in `"..."` if it contains spaces.
//! wait SUBSTR within MS    ; block until SUBSTR appears in output;
//!                          ; fail if not seen within MS milliseconds.
//! sleep MS                 ; unconditional delay (heisenbug-catch).
//! send "STRING"            ; write STRING to QEMU stdin. Supports
//!                          ; \n, \r, \t, \e, \0, \xNN, \\, \".
//!                          ; ESC/control bytes are flushed as keypress
//!                          ; boundaries instead of a single paste burst.
//! expect SUBSTR within MS  ; like `wait`, but scoped to output
//!                          ; produced AFTER the most recent `send`.
//! quit                     ; close stdin and wait for QEMU to exit
//!                          ; cleanly (or kill after grace period).
//! ```
//!
//! Substring matching is plain literal — no regex. For multi-line
//! patterns use a substring of one line.
//!
//! ## Group selection
//!
//! Without `--group`, every group in the script runs in source
//! order. Pass `--group A` (or `--group A,B`) to run only those
//! groups; the setup block always runs first. `--list-groups`
//! parses the script and prints group names without spawning QEMU.
//! `--keep-going` runs every selected group even if an earlier one
//! fails, and reports a per-group summary at the end.
//! `--serial-log PATH` writes the captured QEMU console output to PATH
//! after a sequential run, including failing or stop-after-needle runs.
//!
//! ## Parallel mode
//!
//! `--parallel` boots a dedicated QEMU instance per group. All
//! instances run concurrently (up to `--jobs N`, default 4), each
//! executing the setup block then its own group. Live output
//! mirroring is suppressed; captured output for failed groups is
//! printed after all workers finish. Typical speedup: ~2× for a
//! full 13-group run on a developer laptop (boot time amortised
//! across parallel instances). Groups that are already fast or
//! depend on sequential state (rare) can still use the default
//! sequential path.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::Result;
use crate::image::{alpine_initramfs_name, busybox_initramfs_name};
use crate::target::{Profile, TxTarget};
use crate::util::{
    append_tty_winsize_cmdline, default_boot_mode_for_profile, option_value, optional_option_value,
    resolve_path, validate_boot_mode_value,
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONTROL_KEY_DELAY: Duration = Duration::from_millis(25);
const ESC_KEY_DELAY: Duration = Duration::from_millis(500);
const SEND_SETTLE_DELAY: Duration = Duration::from_millis(500);

pub(crate) fn shell_test(root: &Path, args: Vec<String>) -> Result<()> {
    let target = TxTarget::parse(&option_value(&args, "--target")?)?;
    let profile = Profile::parse(
        &optional_option_value(&args, "--profile").unwrap_or_else(|| "busybox".to_string()),
    )?;
    let script_path = optional_option_value(&args, "--script")
        .ok_or_else(|| "missing required --script PATH".to_string())?;
    let script_path = resolve_path(root, PathBuf::from(script_path));

    let group_filter: Option<Vec<String>> = optional_option_value(&args, "--group").map(|s| {
        s.split(',')
            .map(|part| part.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });
    let list_groups = args.iter().any(|a| a == "--list-groups");
    let keep_going = args.iter().any(|a| a == "--keep-going");
    let parallel = args.iter().any(|a| a == "--parallel");
    let stop_after_needle = optional_option_value(&args, "--stop-after-needle");
    let serial_log =
        optional_option_value(&args, "--serial-log").map(|path| resolve_path(root, path.into()));
    let extra_rv64_ext4: Vec<PathBuf> = option_values(&args, "--extra-rv64-ext4")?
        .into_iter()
        .map(|path| resolve_path(root, PathBuf::from(path)))
        .collect();
    let boot_mode = optional_option_value(&args, "--boot-mode")
        .map(|value| validate_boot_mode_value(&value).map(|()| value))
        .transpose()?;
    let append_cmdline = optional_option_value(&args, "--append-cmdline");
    let smp: usize = optional_option_value(&args, "--smp")
        .map(|s| {
            s.parse::<usize>()
                .map_err(|err| format!("invalid --smp value '{s}': {err}"))
                .and_then(|value| {
                    if value == 0 {
                        Err("--smp must be greater than zero".into())
                    } else {
                        Ok(value)
                    }
                })
        })
        .transpose()?
        .unwrap_or(1);
    let jobs: usize = optional_option_value(&args, "--jobs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .max(1);

    let script_text = fs::read_to_string(&script_path)
        .map_err(|err| format!("failed to read {}: {err}", script_path.display()))?;
    let script = parse_script(&script_text)?;

    if script.setup.is_empty() && script.groups.is_empty() {
        return Err("script is empty (no directives)".into());
    }

    if list_groups {
        if script.groups.is_empty() {
            println!("shell-test: (no groups in script — setup-only)");
        } else {
            println!("shell-test: groups in {}:", script_path.display());
            for g in &script.groups {
                println!("  - {} ({} directives)", g.name, g.directives.len());
            }
        }
        return Ok(());
    }

    if stop_after_needle.is_some() && parallel {
        return Err("--stop-after-needle is only supported in sequential mode".into());
    }
    if serial_log.is_some() && parallel {
        return Err("--serial-log is only supported in sequential mode".into());
    }

    // Validate that every name passed to --group exists in the script.
    if let Some(filter) = &group_filter {
        let known: Vec<&str> = script.groups.iter().map(|g| g.name.as_str()).collect();
        for name in filter {
            if !known.contains(&name.as_str()) {
                return Err(format!(
                    "--group {name:?} not present in script (have: {})",
                    if known.is_empty() {
                        "no groups defined".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            }
        }
    }

    let groups_to_run: Vec<&NamedGroup> = match &group_filter {
        Some(filter) => script
            .groups
            .iter()
            .filter(|g| filter.iter().any(|n| n == &g.name))
            .collect(),
        None => script.groups.iter().collect(),
    };

    println!(
        "shell-test: target={} script={}",
        target.name(),
        script_path.display()
    );
    let setup_count = script.setup.len();
    println!(
        "shell-test: setup={setup_count} directives, groups={}/{} selected",
        groups_to_run.len(),
        script.groups.len()
    );

    // ── Parallel mode ──────────────────────────────────────────────────────
    // Each group boots its own QEMU, runs the setup block, then its group
    // directives, and exits. Up to `jobs` groups run concurrently.
    // Live QEMU output is suppressed during parallel runs; captured output
    // for failed groups is printed after all workers finish.
    if parallel && groups_to_run.len() > 1 {
        let concurrency = jobs.min(groups_to_run.len());
        println!(
            "shell-test: parallel mode — {} workers, {} groups",
            concurrency,
            groups_to_run.len()
        );

        let setup_arc: Arc<Vec<Directive>> = Arc::new(script.setup.clone());
        let groups_arc: Arc<Vec<NamedGroup>> =
            Arc::new(groups_to_run.iter().map(|g| (*g).clone()).collect());
        type GroupResults = Vec<(String, std::result::Result<(), String>)>;
        let results_arc: Arc<Mutex<GroupResults>> = Arc::new(Mutex::new(Vec::new()));
        let next_idx = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..concurrency)
            .map(|_| {
                let setup = Arc::clone(&setup_arc);
                let groups = Arc::clone(&groups_arc);
                let results = Arc::clone(&results_arc);
                let next = Arc::clone(&next_idx);
                let root = root.to_path_buf();
                let extra_rv64_ext4 = extra_rv64_ext4.clone();
                let boot_mode = boot_mode.clone();
                let append_cmdline = append_cmdline.clone();
                thread::spawn(move || {
                    loop {
                        let idx = next.fetch_add(1, Ordering::Relaxed);
                        if idx >= groups.len() {
                            break;
                        }
                        let group = &groups[idx];
                        let run = IsolatedGroupRun {
                            root: &root,
                            target,
                            profile,
                            smp,
                            extra_rv64_ext4: &extra_rv64_ext4,
                            boot_mode: boot_mode.as_deref(),
                            append_cmdline: append_cmdline.as_deref(),
                            setup: &setup,
                            group,
                        };
                        let (captured, group_err) = run_group_isolated(&run);
                        let result = match group_err {
                            None => Ok(()),
                            Some(err) => {
                                println!(
                                    "\n--- [{}] captured output ({} bytes) ---",
                                    group.name,
                                    captured.len()
                                );
                                println!("{captured}");
                                Err(err)
                            }
                        };
                        results.lock().unwrap().push((group.name.clone(), result));
                    }
                })
            })
            .collect();

        for handle in handles {
            let _ = handle.join();
        }

        let group_results = match Arc::try_unwrap(results_arc) {
            Ok(mutex) => mutex.into_inner().unwrap(),
            Err(arc) => arc.lock().unwrap().clone(),
        };

        // Sort results into script order before reporting.
        let mut ordered = group_results;
        ordered.sort_by_key(|(name, _)| {
            groups_arc
                .iter()
                .position(|g| &g.name == name)
                .unwrap_or(usize::MAX)
        });
        let failed_count = ordered.iter().filter(|(_, r)| r.is_err()).count();
        let passed = ordered.len() - failed_count;
        println!("shell-test: groups: {passed} passed, {failed_count} failed");
        for (name, result) in &ordered {
            match result {
                Ok(()) => println!("  ok    {name}"),
                Err(err) => println!("  FAIL  {name}: {err}"),
            }
        }
        return if failed_count == 0 {
            println!("shell-test: ok");
            Ok(())
        } else {
            Err(format!("{failed_count} group(s) failed"))
        };
    }

    // ── Sequential mode (default) ───────────────────────────────────────────
    let qemu_cmd = build_qemu_command(
        root,
        target,
        profile,
        smp,
        &extra_rv64_ext4,
        append_cmdline.as_deref(),
        boot_mode.as_deref(),
    )?;
    println!("shell-test: spawning {}", qemu_cmd.join(" "));

    let Some((program, rest)) = qemu_cmd.split_first() else {
        return Err("empty qemu command".into());
    };
    let mut child = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn {program}: {err}"))?;

    let buffer = Arc::new(Mutex::new(String::new()));
    let _ = spawn_reader(&mut child, "stdout", Arc::clone(&buffer), true);
    let _ = spawn_reader(&mut child, "stderr", Arc::clone(&buffer), true);

    let mut anchor = 0usize;
    let mut group_results: Vec<(String, std::result::Result<(), String>)> = Vec::new();
    let mut quit_seen = false;
    let mut stop_triggered = false;

    let outer = (|| -> Result<()> {
        // Setup always runs. A failure here is fatal regardless of
        // --keep-going (subsequent groups have no usable session).
        if let Some(err) = run_block(
            &mut child,
            &buffer,
            "setup",
            &script.setup,
            &mut anchor,
            true,
            stop_after_needle.as_deref(),
            &mut stop_triggered,
        )? {
            return Err(format!("setup: {err}"));
        }
        if stop_triggered {
            return Ok(());
        }
        if directives_quit(&script.setup) {
            quit_seen = true;
            return Ok(());
        }
        for group in &groups_to_run {
            if quit_seen {
                break;
            }
            let outcome = run_block(
                &mut child,
                &buffer,
                &group.name,
                &group.directives,
                &mut anchor,
                true,
                stop_after_needle.as_deref(),
                &mut stop_triggered,
            )?;
            if stop_triggered {
                quit_seen = true;
            }
            match outcome {
                None => group_results.push((group.name.clone(), Ok(()))),
                Some(err) => {
                    group_results.push((group.name.clone(), Err(err.clone())));
                    if !keep_going {
                        return Err(format!("group {:?}: {}", group.name, err));
                    }
                }
            }
            if directives_quit(&group.directives) {
                quit_seen = true;
            }
            if stop_triggered {
                break;
            }
        }
        Ok(())
    })();

    // Best-effort cleanup. If the script ran `quit`, the child is
    // already gone or being reaped. Otherwise kill the child so
    // QEMU doesn't linger past the test.
    let _ = child.kill();
    let _ = child.wait();

    if let Some(path) = &serial_log {
        write_serial_log(path, &buffer)?;
    }

    let captured_bytes = buffer.lock().unwrap().len();
    let dump_captured = || {
        println!();
        println!("--- captured output ({captured_bytes} bytes) ---");
        println!("{}", buffer.lock().unwrap());
    };

    match outer {
        Ok(()) => {
            if stop_triggered {
                println!(
                    "shell-test: stop needle observed: {}",
                    stop_after_needle.as_deref().unwrap_or_default()
                );
                println!("shell-test: ok");
                return Ok(());
            }
            let failed: Vec<&(String, std::result::Result<(), String>)> =
                group_results.iter().filter(|(_, r)| r.is_err()).collect();
            if !group_results.is_empty() {
                let passed = group_results.len() - failed.len();
                println!(
                    "shell-test: groups: {} passed, {} failed",
                    passed,
                    failed.len()
                );
                for (name, result) in &group_results {
                    match result {
                        Ok(()) => println!("  ok    {name}"),
                        Err(err) => println!("  FAIL  {name}: {err}"),
                    }
                }
            }
            if failed.is_empty() {
                println!("shell-test: ok");
                Ok(())
            } else {
                dump_captured();
                Err(format!("{} group(s) failed", failed.len()))
            }
        }
        Err(err) => {
            dump_captured();
            Err(err)
        }
    }
}

fn write_serial_log(path: &Path, buffer: &Arc<Mutex<String>>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let captured = buffer.lock().unwrap().clone();
    fs::write(path, captured).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn directives_quit(d: &[Directive]) -> bool {
    matches!(d.last(), Some(Directive::Quit))
}

/// Run one block (the setup block or one named group). Returns
/// `Ok(None)` if every directive succeeded, `Ok(Some(err))` for a
/// directive-level failure (caller decides whether to keep going),
/// and `Err(_)` for harness-level failures (broken stdin pipe, etc.)
/// that prevent the session from continuing at all.
///
/// When `verbose` is false the per-directive progress lines are
/// suppressed (used by isolated parallel instances whose output
/// would otherwise interleave on the host terminal).
fn run_block(
    child: &mut Child,
    buffer: &Arc<Mutex<String>>,
    block_label: &str,
    directives: &[Directive],
    script_anchor: &mut usize,
    verbose: bool,
    stop_after_needle: Option<&str>,
    stop_triggered: &mut bool,
) -> Result<Option<String>> {
    if directives.is_empty() {
        return Ok(None);
    }
    if verbose {
        println!("[{block_label}]");
    }
    for (idx, directive) in directives.iter().enumerate() {
        if verbose {
            println!("  [{idx}] {}", directive.summary());
        }
        match directive {
            Directive::Group(_) => {
                // Group markers are consumed by `parse_script` and never
                // appear inside a block.
                unreachable!("Directive::Group should not survive parse_script");
            }
            Directive::Wait { needle, timeout } => {
                if let Err(err) = wait_for(buffer, 0, needle, *timeout) {
                    return Ok(Some(err));
                }
            }
            Directive::Sleep(ms) => {
                thread::sleep(Duration::from_millis(*ms));
            }
            Directive::Send(text) => {
                thread::sleep(SEND_SETTLE_DELAY);
                let stdin = child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| "qemu stdin pipe missing".to_string())?;
                // Snapshot the buffer length so subsequent `expect`
                // directives only match output produced AFTER this
                // send.
                *script_anchor = buffer.lock().unwrap().len();
                send_interactive_bytes(stdin, text.as_bytes())?;
            }
            Directive::Expect { needle, timeout } => {
                if let Err(err) = wait_for(buffer, *script_anchor, needle, *timeout) {
                    return Ok(Some(err));
                }
            }
            Directive::Quit => {
                // Drop the kernel pipe and kill QEMU. We don't wait
                // for graceful exit because `-serial mon:stdio`
                // keeps QEMU alive even after stdin closes — the
                // guest is still running, the host TTY just sees
                // EOF on its side. The test has already verified
                // whatever behaviour it cares about; killing here
                // is the test-harness equivalent of the user
                // hitting Ctrl-A X.
                if let Some(stdin) = child.stdin.take() {
                    drop(stdin);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
        }
        if let Some(needle) = stop_after_needle {
            if buffer.lock().unwrap().contains(needle) {
                if let Some(stdin) = child.stdin.take() {
                    drop(stdin);
                }
                let _ = child.kill();
                let _ = child.wait();
                *stop_triggered = true;
                return Ok(None);
            }
        }
    }
    Ok(None)
}

fn wait_for(
    buffer: &Arc<Mutex<String>>,
    start_offset: usize,
    needle: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let b = buffer.lock().unwrap();
            if let Some(haystack) = b.get(start_offset..) {
                if haystack.contains(needle) {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {} ms waiting for {:?}",
                timeout.as_millis(),
                needle
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn send_interactive_bytes(stdin: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let mut start = 0usize;
    for (idx, &byte) in bytes.iter().enumerate() {
        let delay = key_boundary_delay(byte);
        if delay.is_none() {
            continue;
        }
        if start < idx {
            stdin
                .write_all(&bytes[start..idx])
                .map_err(|err| format!("write to qemu stdin: {err}"))?;
        }
        stdin
            .write_all(&[byte])
            .map_err(|err| format!("write to qemu stdin: {err}"))?;
        stdin
            .flush()
            .map_err(|err| format!("flush qemu stdin: {err}"))?;
        thread::sleep(delay.unwrap());
        start = idx + 1;
    }
    if start < bytes.len() {
        stdin
            .write_all(&bytes[start..])
            .map_err(|err| format!("write to qemu stdin: {err}"))?;
    }
    stdin
        .flush()
        .map_err(|err| format!("flush qemu stdin: {err}"))?;
    Ok(())
}

fn key_boundary_delay(byte: u8) -> Option<Duration> {
    match byte {
        0x1b => Some(ESC_KEY_DELAY),
        0x00..=0x08 | 0x0b..=0x1a | 0x1c..=0x1f | 0x7f => Some(CONTROL_KEY_DELAY),
        _ => None,
    }
}

/// Drain one of QEMU's output streams into `buffer`.
///
/// When `live_mirror` is true each byte is also echoed to the host
/// terminal in real time (sequential mode). Pass `false` in parallel
/// mode to avoid interleaved output from concurrent QEMU instances.
///
/// Returns the thread handle so callers can join it when they need
/// the buffer to be fully populated (e.g. before reporting failures
/// in parallel mode).
fn spawn_reader(
    child: &mut Child,
    kind: &'static str,
    buffer: Arc<Mutex<String>>,
    live_mirror: bool,
) -> thread::JoinHandle<()> {
    let stream: Box<dyn std::io::Read + Send> = match kind {
        "stdout" => match child.stdout.take() {
            Some(s) => Box::new(s),
            None => return thread::spawn(|| {}),
        },
        "stderr" => match child.stderr.take() {
            Some(s) => Box::new(s),
            None => return thread::spawn(|| {}),
        },
        _ => return thread::spawn(|| {}),
    };
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        let mut buf = Vec::new();
        let mut pending = VecDeque::<u8>::new();
        for byte in reader.bytes() {
            let Ok(b) = byte else { break };
            if live_mirror {
                let _ = std::io::stdout().write_all(&[b]);
                let _ = std::io::stdout().flush();
            }
            pending.push_back(b);
            while let Some(b) = pending.pop_front() {
                buf.push(b);
            }
            let s = String::from_utf8_lossy(&buf);
            buffer.lock().unwrap().push_str(&s);
            buf.clear();
        }
    })
}

#[derive(Debug, Clone)]
enum Directive {
    Group(String),
    Wait { needle: String, timeout: Duration },
    Sleep(u64),
    Send(String),
    Expect { needle: String, timeout: Duration },
    Quit,
}

impl Directive {
    fn summary(&self) -> String {
        match self {
            Self::Group(name) => format!("group {name:?}"),
            Self::Wait { needle, timeout } => {
                format!("wait {:?} within {} ms", needle, timeout.as_millis())
            }
            Self::Sleep(ms) => format!("sleep {ms} ms"),
            Self::Send(text) => format!("send {text:?}"),
            Self::Expect { needle, timeout } => {
                format!("expect {:?} within {} ms", needle, timeout.as_millis())
            }
            Self::Quit => "quit".into(),
        }
    }
}

#[derive(Debug, Clone)]
struct NamedGroup {
    name: String,
    directives: Vec<Directive>,
}

#[derive(Debug, Default)]
struct Script {
    /// Directives that appear before the first `group` line. Always
    /// executed, regardless of `--group` filtering.
    setup: Vec<Directive>,
    groups: Vec<NamedGroup>,
}

fn parse_script(text: &str) -> Result<Script> {
    let mut script = Script::default();
    let mut current: Option<NamedGroup> = None;
    let mut seen_names: Vec<String> = Vec::new();

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed =
            parse_line(line).map_err(|err| format!("script line {}: {err}", lineno + 1))?;
        match parsed {
            Directive::Group(name) => {
                if name.is_empty() {
                    return Err(format!("script line {}: empty group name", lineno + 1));
                }
                if seen_names.iter().any(|n| n == &name) {
                    return Err(format!(
                        "script line {}: duplicate group name {name:?}",
                        lineno + 1
                    ));
                }
                seen_names.push(name.clone());
                if let Some(prev) = current.take() {
                    script.groups.push(prev);
                }
                current = Some(NamedGroup {
                    name,
                    directives: Vec::new(),
                });
            }
            other => match current.as_mut() {
                Some(group) => group.directives.push(other),
                None => script.setup.push(other),
            },
        }
    }
    if let Some(last) = current.take() {
        script.groups.push(last);
    }
    Ok(script)
}

fn parse_line(line: &str) -> std::result::Result<Directive, String> {
    if line == "quit" {
        return Ok(Directive::Quit);
    }
    if let Some(rest) = line.strip_prefix("group ") {
        let name = parse_group_name(rest)?;
        return Ok(Directive::Group(name));
    }
    if let Some(rest) = line.strip_prefix("sleep ") {
        let ms: u64 = rest
            .trim()
            .parse()
            .map_err(|err| format!("sleep: invalid number '{}': {err}", rest.trim()))?;
        return Ok(Directive::Sleep(ms));
    }
    if let Some(rest) = line.strip_prefix("send ") {
        let s = parse_quoted(rest.trim())?;
        return Ok(Directive::Send(s));
    }
    if let Some(rest) = line.strip_prefix("wait ") {
        let (needle, timeout) = parse_pattern_with_timeout(rest)?;
        return Ok(Directive::Wait { needle, timeout });
    }
    if let Some(rest) = line.strip_prefix("expect ") {
        let (needle, timeout) = parse_pattern_with_timeout(rest)?;
        return Ok(Directive::Expect { needle, timeout });
    }
    Err(format!("unknown directive '{line}'"))
}

/// Parse the value half of a `group ...` directive. Accept either a
/// quoted string (`group "file ops"`) for names that need spaces or
/// special characters, or a bare identifier-like name (everything
/// after `group ` trimmed) for the common case.
fn parse_group_name(rest: &str) -> std::result::Result<String, String> {
    let rest = rest.trim();
    if rest.starts_with('"') {
        parse_quoted(rest)
    } else {
        Ok(rest.to_string())
    }
}

fn parse_pattern_with_timeout(rest: &str) -> std::result::Result<(String, Duration), String> {
    // Format: `"NEEDLE" within MS`
    let (needle_part, after) = split_quoted_prefix(rest.trim())?;
    let after = after.trim();
    let timeout_str = after
        .strip_prefix("within ")
        .ok_or_else(|| "missing `within MS` clause".to_string())?
        .trim();
    let ms: u64 = timeout_str
        .parse()
        .map_err(|err| format!("within: invalid number '{timeout_str}': {err}"))?;
    Ok((needle_part, Duration::from_millis(ms)))
}

fn parse_quoted(s: &str) -> std::result::Result<String, String> {
    let (parsed, rest) = split_quoted_prefix(s)?;
    if !rest.trim().is_empty() {
        return Err(format!("trailing garbage after quoted string: {rest:?}"));
    }
    Ok(parsed)
}

fn split_quoted_prefix(s: &str) -> std::result::Result<(String, &str), String> {
    let s = s.trim_start();
    let mut chars = s.char_indices();
    if chars.next().map(|(_, c)| c) != Some('"') {
        return Err("expected '\"' to start string literal".into());
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                let after = &s[i + 1..];
                return Ok((out, after));
            }
            '\\' => {
                let (_, esc) = chars
                    .next()
                    .ok_or_else(|| "unterminated escape at end of string".to_string())?;
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'e' => out.push('\x1b'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '0' => out.push('\0'),
                    'x' => {
                        let (_, hi) = chars.next().ok_or_else(|| {
                            "unterminated hex escape; expected two digits".to_string()
                        })?;
                        let (_, lo) = chars.next().ok_or_else(|| {
                            "unterminated hex escape; expected two digits".to_string()
                        })?;
                        let value = hex_byte(hi, lo)
                            .ok_or_else(|| format!("invalid hex escape '\\x{hi}{lo}'"))?;
                        out.push(value as char);
                    }
                    other => return Err(format!("unknown escape '\\{other}'")),
                }
            }
            other => out.push(other),
        }
    }
    Err("unterminated string literal (no closing '\"')".into())
}

fn hex_byte(hi: char, lo: char) -> Option<u8> {
    let hi = hi.to_digit(16)?;
    let lo = lo.to_digit(16)?;
    Some(((hi << 4) | lo) as u8)
}

struct IsolatedGroupRun<'a> {
    root: &'a Path,
    target: TxTarget,
    profile: Profile,
    smp: usize,
    extra_rv64_ext4: &'a [PathBuf],
    boot_mode: Option<&'a str>,
    append_cmdline: Option<&'a str>,
    setup: &'a [Directive],
    group: &'a NamedGroup,
}

/// Boot an isolated QEMU instance, run the setup block, then run
/// exactly one group, and return `(captured_output, error_or_none)`.
///
/// Designed for `--parallel` mode: live output mirroring is suppressed
/// so parallel instances don't interleave on the host terminal. The
/// reader threads are joined before the buffer is read so the caller
/// always gets the complete output.
///
/// Returns `(output, None)` on success, `(output, Some(msg))` on failure
/// (both directive failures and harness failures like QEMU spawn errors).
fn run_group_isolated(run: &IsolatedGroupRun<'_>) -> (String, Option<String>) {
    let buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let group_err = run_group_isolated_inner(run, Arc::clone(&buffer));
    let captured = buffer.lock().unwrap().clone();
    (captured, group_err)
}

fn run_group_isolated_inner(
    run: &IsolatedGroupRun<'_>,
    buffer: Arc<Mutex<String>>,
) -> Option<String> {
    let qemu_cmd = match build_qemu_command(
        run.root,
        run.target,
        run.profile,
        run.smp,
        run.extra_rv64_ext4,
        run.append_cmdline,
        run.boot_mode,
    ) {
        Ok(c) => c,
        Err(e) => return Some(format!("qemu command: {e}")),
    };
    let Some((program, rest)) = qemu_cmd.split_first() else {
        return Some("empty qemu command".into());
    };
    let mut child = match Command::new(program)
        .args(rest)
        .current_dir(run.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Some(format!("spawn qemu for {:?}: {e}", run.group.name)),
    };

    let h_out = spawn_reader(&mut child, "stdout", Arc::clone(&buffer), false);
    let h_err = spawn_reader(&mut child, "stderr", Arc::clone(&buffer), false);

    let mut anchor = 0usize;
    let mut stop_triggered = false;

    let result = (|| -> Option<String> {
        match run_block(
            &mut child,
            &buffer,
            "setup",
            run.setup,
            &mut anchor,
            false,
            None,
            &mut stop_triggered,
        ) {
            Err(e) => return Some(format!("setup (harness): {e}")),
            Ok(Some(e)) => return Some(format!("setup: {e}")),
            Ok(None) => {}
        }
        match run_block(
            &mut child,
            &buffer,
            &run.group.name,
            &run.group.directives,
            &mut anchor,
            false,
            None,
            &mut stop_triggered,
        ) {
            Err(e) => Some(format!("group harness: {e}")),
            Ok(v) => v,
        }
    })();

    let _ = child.kill();
    let _ = child.wait();
    // Join readers so the buffer is fully populated before the caller reads it.
    let _ = h_out.join();
    let _ = h_err.join();

    result
}

fn build_qemu_command(
    root: &Path,
    target: TxTarget,
    profile: Profile,
    smp: usize,
    extra_rv64_ext4: &[PathBuf],
    append_cmdline: Option<&str>,
    boot_mode: Option<&str>,
) -> Result<Vec<String>> {
    // Reuse the existing qemu_command builder by constructing an
    // args list and invoking the same dispatcher path. We can't call
    // the private `qemu_command` directly without exposing it; build
    // the command inline instead, matching the busybox profile +
    // --interactive flag.
    let kernel = target.kernel_path(root);
    let initramfs_name = match profile {
        Profile::Busybox => busybox_initramfs_name(target),
        Profile::Alpine => alpine_initramfs_name(target),
        Profile::Smoke => {
            return Err("shell-test supports busybox or alpine profiles, not smoke".into());
        }
    };
    let initramfs = root.join("target").join("images").join(initramfs_name);
    let mut args = vec![
        target.qemu_binary().to_string(),
        "-machine".into(),
        target.qemu_machine().to_string(),
        "-m".into(),
        match (target, profile) {
            (TxTarget::La64Qemu, _) => "1152M",
            (TxTarget::Rv64Qemu, Profile::Alpine) => "1024M",
            (TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock, _) => "256M",
        }
        .into(),
        "-smp".into(),
        smp.to_string(),
        "-accel".into(),
        "tcg,thread=multi".into(),
        "-display".into(),
        "none".into(),
        "-serial".into(),
        "mon:stdio".into(),
        "-no-reboot".into(),
        "-kernel".into(),
        kernel.display().to_string(),
    ];
    if matches!(target, TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock) {
        args.push("-bios".into());
        args.push("default".into());
    }
    args.push("-initrd".into());
    args.push(initramfs.display().to_string());
    args.push("-append".into());
    let boot_mode = boot_mode.unwrap_or_else(|| default_boot_mode_for_profile(profile));
    let cmdline_base = match profile {
        Profile::Busybox => format!("tx.profile=busybox tx.boot.mode={boot_mode} console=ttyS0"),
        Profile::Alpine => {
            format!(
                "tx.profile=alpine tx.boot.mode={boot_mode} init=/bin/tx-bootstrap-busybox console=ttyS0"
            )
        }
        Profile::Smoke => unreachable!("rejected above"),
    };
    let cmdline = match append_cmdline {
        Some(extra) if !extra.trim().is_empty() => format!("{cmdline_base} {}", extra.trim()),
        _ => cmdline_base,
    };
    args.push(append_tty_winsize_cmdline(&cmdline));
    if !extra_rv64_ext4.is_empty() && target != TxTarget::Rv64Qemu {
        return Err("--extra-rv64-ext4 is only supported for rv64-qemu".into());
    }
    if extra_rv64_ext4.len() > 3 {
        return Err("--extra-rv64-ext4 supports at most three RV64 virtio-mmio drives".into());
    }
    for (idx, path) in extra_rv64_ext4.iter().enumerate() {
        args.push("-drive".into());
        args.push(format!(
            "file={},format=raw,if=none,id=txblk{idx}",
            path.display(),
        ));
        args.push("-device".into());
        args.push(format!(
            "virtio-blk-device,drive=txblk{idx},bus=virtio-mmio-bus.{idx}"
        ));
    }
    Ok(args)
}

fn option_values(args: &[String], name: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == name {
            let Some(value) = args.get(idx + 1) else {
                return Err(format!("option {name} needs a value"));
            };
            values.push(value.clone());
            idx += 2;
        } else {
            idx += 1;
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn alpine_profile_uses_alpine_initramfs_and_cmdline() {
        let root = Path::new("/tmp/tx");
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            1,
            &[],
            None,
            None,
        )
        .expect("build qemu command");
        let rendered = command.join(" ");

        assert!(rendered.contains("alpine-initramfs-rv64-qemu.cpio"));
        assert!(rendered.contains(
            "tx.profile=alpine tx.boot.mode=alpine init=/bin/tx-bootstrap-busybox console=ttyS0"
        ));
        assert!(rendered.contains("tx.tty.rows="));
        assert!(rendered.contains("tx.tty.cols="));
        assert!(rendered.contains("-m 1024M"));
        assert!(rendered.contains("-smp 1"));
        assert!(!rendered.contains("busybox-initramfs-rv64-qemu.cpio"));
    }

    #[test]
    fn shell_test_qemu_command_honors_smp_override() {
        let root = Path::new("/tmp/tx");
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            4,
            &[],
            None,
            None,
        )
        .expect("build qemu command");
        assert!(command.join(" ").contains("-smp 4"));
    }

    #[test]
    fn shell_test_can_attach_rv64_ext4_drive_on_bus0() {
        let root = Path::new("/tmp/tx");
        let image = PathBuf::from("/tmp/tx/target/images/alpine-tcc-dev-root-rv64-qemu.ext4");
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            1,
            &[image],
            None,
            None,
        )
        .expect("build qemu command");
        let rendered = command.join(" ");

        assert!(rendered.contains("-drive file=/tmp/tx/target/images/alpine-tcc-dev-root-rv64-qemu.ext4,format=raw,if=none,id=txblk0"));
        assert!(rendered.contains("-device virtio-blk-device,drive=txblk0,bus=virtio-mmio-bus.0"));
    }

    #[test]
    fn shell_test_can_attach_three_rv64_ext4_drives_on_stable_buses() {
        let root = Path::new("/tmp/tx");
        let images = vec![
            PathBuf::from("/tmp/tx/test.img"),
            PathBuf::from("/tmp/tx/scratch.img"),
            PathBuf::from("/tmp/tx/workload.img"),
        ];
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            1,
            &images,
            None,
            None,
        )
        .expect("build qemu command");
        let rendered = command.join(" ");

        for (idx, name) in ["test", "scratch", "workload"].iter().enumerate() {
            assert!(rendered.contains(&format!(
                "-drive file=/tmp/tx/{name}.img,format=raw,if=none,id=txblk{idx}"
            )));
            assert!(rendered.contains(&format!(
                "-device virtio-blk-device,drive=txblk{idx},bus=virtio-mmio-bus.{idx}"
            )));
        }
    }

    #[test]
    fn shell_test_rejects_serial_log_parallel_mode() {
        let root = temp_root("serial-log-parallel");
        let script = root.join("script.scn");
        fs::create_dir_all(&root).expect("create temp root");
        fs::write(
            &script,
            r#"
group one
wait "never" within 1
group two
wait "never" within 1
"#,
        )
        .expect("write script");

        let error = shell_test(
            &root,
            vec![
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "alpine".into(),
                "--script".into(),
                script.display().to_string(),
                "--parallel".into(),
                "--serial-log".into(),
                root.join("serial.log").display().to_string(),
            ],
        )
        .expect_err("parallel serial log must be rejected before qemu");

        assert!(error.contains("--serial-log is only supported in sequential mode"));
    }

    #[test]
    fn shell_test_can_append_kernel_cmdline_tokens() {
        let root = Path::new("/tmp/tx");
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            1,
            &[],
            Some("tx.mount.sdcard=0"),
            None,
        )
        .expect("build qemu command");
        let rendered = command.join(" ");

        assert!(rendered.contains("tx.mount.sdcard=0"));
        assert!(rendered.contains("tx.tty.rows="));
        assert!(rendered.contains("tx.tty.cols="));
    }

    #[test]
    fn shell_test_boot_mode_override_replaces_profile_default() {
        let root = Path::new("/tmp/tx");
        let command = build_qemu_command(
            root,
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            1,
            &[],
            None,
            Some("contest"),
        )
        .expect("build qemu command");
        let rendered = command.join(" ");

        assert!(rendered.contains("tx.profile=alpine"));
        assert!(rendered.contains("tx.boot.mode=contest"));
        assert!(!rendered.contains("tx.boot.mode=alpine"));
    }

    #[test]
    fn quoted_send_supports_hex_control_bytes() {
        let parsed = parse_quoted(r#""a\x03\e""#).expect("parse quoted");
        assert_eq!(parsed.as_bytes(), &[b'a', 0x03, 0x1b]);
    }

    #[test]
    fn send_interactive_bytes_preserves_order_across_escape_boundary() {
        let mut out = Vec::new();

        send_interactive_bytes(&mut out, b"iabc\n\x1b:wq\n").expect("send bytes");

        assert_eq!(out, b"iabc\n\x1b:wq\n");
    }

    #[test]
    fn send_interactive_bytes_waits_after_escape_boundary() {
        let mut out = Vec::new();
        let start = Instant::now();

        send_interactive_bytes(&mut out, b"\x1b:wq\n").expect("send bytes");

        assert_eq!(out, b"\x1b:wq\n");
        assert!(start.elapsed() >= ESC_KEY_DELAY);
    }

    fn temp_root(suffix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tx-shell-test-{suffix}-{}-{unique}",
            std::process::id()
        ))
    }
}
