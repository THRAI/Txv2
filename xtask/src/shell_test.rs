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

use std::collections::VecDeque;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

    let script_text = fs::read_to_string(&script_path)
        .map_err(|err| format!("failed to read {}: {err}", script_path.display()))?;
    let directives = parse_script(&script_text)?;

    if directives.is_empty() {
        return Err("script is empty (no directives)".into());
    }

    println!(
        "shell-test: target={} script={}",
        target.name(),
        script_path.display()
    );
    println!("shell-test: {} directives", directives.len());

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
    spawn_reader(&mut child, "stdout", Arc::clone(&buffer));
    spawn_reader(&mut child, "stderr", Arc::clone(&buffer));

    let mut script_anchor = 0usize;
    let result = run_script(&mut child, &buffer, &directives, &mut script_anchor);

    // Best-effort cleanup. If the script ran `quit`, the child is
    // already gone or being reaped. Otherwise kill the child so
    // QEMU doesn't linger past the test.
    let _ = child.kill();
    let _ = child.wait();

    if let Err(err) = result {
        println!();
        println!(
            "--- captured output ({} bytes) ---",
            buffer.lock().unwrap().len()
        );
        println!("{}", buffer.lock().unwrap());
        return Err(err);
    }

    println!("shell-test: ok");
    Ok(())
}

fn run_script(
    child: &mut Child,
    buffer: &Arc<Mutex<String>>,
    directives: &[Directive],
    script_anchor: &mut usize,
) -> Result<()> {
    for (idx, directive) in directives.iter().enumerate() {
        println!("[{idx}] {}", directive.summary());
        match directive {
            Directive::Wait { needle, timeout } => {
                wait_for(buffer, 0, needle, *timeout)?;
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
                wait_for(buffer, *script_anchor, needle, *timeout)?;
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
                return Ok(());
            }
        }
    }
    Ok(())
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

fn spawn_reader(child: &mut Child, kind: &'static str, buffer: Arc<Mutex<String>>) {
    let stream: Box<dyn std::io::Read + Send> = match kind {
        "stdout" => match child.stdout.take() {
            Some(s) => Box::new(s),
            None => return,
        },
        "stderr" => match child.stderr.take() {
            Some(s) => Box::new(s),
            None => return,
        },
        _ => return,
    };
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        let mut buf = Vec::new();
        let mut pending = VecDeque::<u8>::new();
        for byte in reader.bytes() {
            let Ok(b) = byte else { break };
            // Mirror to host stdout so the user sees QEMU output live.
            let _ = std::io::stdout().write_all(&[b]);
            let _ = std::io::stdout().flush();
            pending.push_back(b);
            // Drain into the captured buffer in chunks. We use
            // String for substring matching; non-UTF-8 bytes are
            // replaced with the U+FFFD replacement char.
            while let Some(b) = pending.pop_front() {
                buf.push(b);
            }
            let s = String::from_utf8_lossy(&buf);
            buffer.lock().unwrap().push_str(&s);
            buf.clear();
        }
    });
}

#[derive(Debug)]
enum Directive {
    Wait { needle: String, timeout: Duration },
    Sleep(u64),
    Send(String),
    Expect { needle: String, timeout: Duration },
    Quit,
}

impl Directive {
    fn summary(&self) -> String {
        match self {
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

fn parse_script(text: &str) -> Result<Vec<Directive>> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed =
            parse_line(line).map_err(|err| format!("script line {}: {err}", lineno + 1))?;
        out.push(parsed);
    }
    Ok(out)
}

fn parse_line(line: &str) -> std::result::Result<Directive, String> {
    if line == "quit" {
        return Ok(Directive::Quit);
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
