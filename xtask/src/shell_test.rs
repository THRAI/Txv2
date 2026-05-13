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
//!   to surface heisenbugs (e.g. "sending input the instant the
//!   prompt appears, before the read syscall registers"). The
//!   script author owns the pause discipline; the driver does NOT
//!   silently coalesce input into a sustained burst.
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
//! send "STRING"            ; write STRING to QEMU stdin. Standard
//!                          ; rust escapes (\n, \r, \t, \\, \").
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

use crate::target::{Profile, TxTarget};
use crate::util::{option_value, optional_option_value, resolve_path};
use crate::Result;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn shell_test(root: &Path, args: Vec<String>) -> Result<()> {
    let target = TxTarget::parse(&option_value(&args, "--target")?)?;
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
                thread::spawn(move || loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= groups.len() {
                        break;
                    }
                    let group = &groups[idx];
                    let (captured, group_err) = run_group_isolated(&root, target, &setup, group);
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
        println!(
            "shell-test: groups: {} passed, {} failed",
            passed, failed_count,
        );
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
    let qemu_cmd = build_qemu_command(root, target)?;
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
        )? {
            return Err(format!("setup: {err}"));
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
            )?;
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
        }
        Ok(())
    })();

    // Best-effort cleanup. If the script ran `quit`, the child is
    // already gone or being reaped. Otherwise kill the child so
    // QEMU doesn't linger past the test.
    let _ = child.kill();
    let _ = child.wait();

    let captured_bytes = buffer.lock().unwrap().len();
    let dump_captured = || {
        println!();
        println!("--- captured output ({captured_bytes} bytes) ---");
        println!("{}", buffer.lock().unwrap());
    };

    match outer {
        Ok(()) => {
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
                let stdin = child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| "qemu stdin pipe missing".to_string())?;
                // Snapshot the buffer length so subsequent `expect`
                // directives only match output produced AFTER this
                // send.
                *script_anchor = buffer.lock().unwrap().len();
                stdin
                    .write_all(text.as_bytes())
                    .map_err(|err| format!("write to qemu stdin: {err}"))?;
                stdin
                    .flush()
                    .map_err(|err| format!("flush qemu stdin: {err}"))?;
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
            Self::Send(text) => format!("send {:?}", text),
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
        return Err(format!("trailing garbage after quoted string: {:?}", rest));
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
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '0' => out.push('\0'),
                    other => return Err(format!("unknown escape '\\{other}'")),
                }
            }
            other => out.push(other),
        }
    }
    Err("unterminated string literal (no closing '\"')".into())
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
fn run_group_isolated(
    root: &Path,
    target: TxTarget,
    setup: &[Directive],
    group: &NamedGroup,
) -> (String, Option<String>) {
    let buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let group_err = run_group_isolated_inner(root, target, setup, group, Arc::clone(&buffer));
    let captured = buffer.lock().unwrap().clone();
    (captured, group_err)
}

fn run_group_isolated_inner(
    root: &Path,
    target: TxTarget,
    setup: &[Directive],
    group: &NamedGroup,
    buffer: Arc<Mutex<String>>,
) -> Option<String> {
    let qemu_cmd = match build_qemu_command(root, target) {
        Ok(c) => c,
        Err(e) => return Some(format!("qemu command: {e}")),
    };
    let Some((program, rest)) = qemu_cmd.split_first() else {
        return Some("empty qemu command".into());
    };
    let mut child = match Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Some(format!("spawn qemu for {:?}: {e}", group.name)),
    };

    let h_out = spawn_reader(&mut child, "stdout", Arc::clone(&buffer), false);
    let h_err = spawn_reader(&mut child, "stderr", Arc::clone(&buffer), false);

    let mut anchor = 0usize;

    let result = (|| -> Option<String> {
        match run_block(&mut child, &buffer, "setup", setup, &mut anchor, false) {
            Err(e) => return Some(format!("setup (harness): {e}")),
            Ok(Some(e)) => return Some(format!("setup: {e}")),
            Ok(None) => {}
        }
        match run_block(
            &mut child,
            &buffer,
            &group.name,
            &group.directives,
            &mut anchor,
            false,
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

fn build_qemu_command(root: &Path, target: TxTarget) -> Result<Vec<String>> {
    // Reuse the existing qemu_command builder by constructing an
    // args list and invoking the same dispatcher path. We can't call
    // the private `qemu_command` directly without exposing it; build
    // the command inline instead, matching the busybox profile +
    // --interactive flag.
    let kernel = target.kernel_path(root);
    let initramfs = root
        .join("target")
        .join("images")
        .join("busybox-initramfs.cpio");
    let mut args = vec![
        target.qemu_binary().to_string(),
        "-machine".into(),
        target.qemu_machine().to_string(),
        "-m".into(),
        "256M".into(),
        "-smp".into(),
        "1".into(),
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
    args.push("tx.profile=busybox console=ttyS0".into());
    let _ = Profile::Busybox; // documentation: this driver always uses busybox.
    Ok(args)
}
