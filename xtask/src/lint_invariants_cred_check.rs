//! Lint: every cred-mutating syscall arm must call a cred-check
//! function before invoking a mutator primitive.
//!
//! Per `cred_service_v_1` §"Minting rule" and the
//! `docs/design/02_execution/cred_snapshot_wiring_v_1.md` companion:
//! syscall arms in `crates/tx-shims/src/linux_syscall/` that drive a
//! known cred-relevant mutator (FS rename / unlink / link / chmod /
//! chown / signal-send / per-thread kill / etc.) must pass through
//! a `cred::checks::require_*` predicate or a cred-checked
//! `signal::script_*` script.
//!
//! The lint enforces "every mutator is gated by a cred check in the
//! same function body". It is intentionally syntactic (no type info)
//! so it runs as part of `xtask lint` without dragging in
//! rust-analyzer machinery.
//!
//! Detection model:
//!
//! 1. Walk every `pub(super) (async )?fn sys_*` in
//!    `crates/tx-shims/src/linux_syscall/` (excluding mod.rs / numbers.rs
//!    / tests/).
//! 2. Extract the function body via brace-depth tracking.
//! 3. If the body mentions any [`MUTATOR_SIGNALS`] string AND mentions
//!    NO [`CRED_CHECK_SIGNALS`] string, flag the function.
//! 4. Allow-list a handful of functions whose mutation is
//!    architecturally self-only (e.g. `sys_tgkill`'s
//!    tgid-must-match-caller-pid constraint), where a cred check would
//!    be trivially permitted.
//!
//! No ratchet — the lint fails on any violation. The audit landed
//! the cred wiring for every flagged mutator before this lint, so
//! the floor is zero from day one.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Strings whose presence in a `sys_*` body indicates the syscall
/// invokes a cred-relevant mutator. Match is substring-based, so each
/// entry should be specific enough to avoid false positives but
/// generic enough to catch reasonable variants.
///
/// Adding entries: when a new mutator primitive or StepOp wrap is
/// introduced that mutates cred-protected state (signal-send,
/// fs-mutate, process-control on another subject), add it here.
const MUTATOR_SIGNALS: &[&str] = &[
    // Signal-send primitives + StepOp wraps. Direct calls to these
    // would bypass cred::require_signal_send.
    "step_kill_process(",
    "step_kill_pgrp(",
    "deliver_posix_signal(",
    "KillProcessOp",
    "KillPgrpOp",
    "DeliverSignalOp",
    "ThreadKillOp",
    // FS mutators on directory contents (require write-on-parent,
    // sometimes sticky-bit rule).
    "fs_ops.unlink(",
    "fs_ops.rmdir(",
    "fs_ops.link(",
    "fs_ops.rename(",
    "fs_ops.create_inode(",
    "fs_ops.mkdir(",
    "fs_ops.symlink(",
    // FS mutators on inode metadata (require ownership or CAP_FOWNER).
    "fs_ops.chmod_inode(",
    "fs_ops.chown_inode(",
    "fs_ops.step_truncate(",
    // Composite StepOp wraps that internally drive the FS mutators.
    "RenameOp {",
    "ChmodOp {",
    "ChownOp {",
    "MkdirOp {",
    "SymlinkOp {",
    "TruncateOp {",
    "LinkOp {",
    "UnlinkOp {",
];

/// Strings whose presence proves the function ran a cred check
/// before reaching its mutator dispatch. Substring match.
///
/// Both `require_*` (witness-producing predicates) and `authorize_*`
/// (combinators) satisfy. `script_kill_*` and `script_deliver_signal`
/// are cred-checked entry points in the signal subsystem that wrap
/// the primitives — calling them is equivalent to running a cred
/// check.
const CRED_CHECK_SIGNALS: &[&str] = &[
    // cred::checks::* witness predicates and combinators. Matches
    // both fully-qualified (`tx_subsystems::cred::checks::X`) and
    // partially-qualified (`cred::checks::X` after `use
    // tx_subsystems::cred;`) call sites.
    "cred::checks::require_",
    "cred::checks::authorize_",
    // Aliased prefix (`use tx_subsystems::cred::checks as cred_checks;`).
    // The alias-name MUST be `cred_checks` to remain lint-recognisable.
    // Other aliases (e.g. `use ... as checks`) would silently bypass
    // the gate — keep the alias canonical.
    "cred_checks::require_",
    "cred_checks::authorize_",
    // Direct cred-root re-exports (legacy call sites; the migration
    // pulled most through cred::checks but a few keep the old path).
    "cred::require_signal_send(",
    "cred::require_path_search(",
    "cred::require_open(",
    "cred::require_unlink(",
    "cred::require_link(",
    "cred::require_rename(",
    "cred::require_chmod(",
    "cred::require_chown(",
    // Signal-subsystem cred-checked script entry points.
    "signal::script_kill_process(",
    "signal::script_kill_pgrp(",
    "signal::script_kill_probe(",
    "signal::script_deliver_signal(",
];

/// Syscall arms whose mutation is architecturally self-only or
/// otherwise outside the cred-check audit scope. Entries are the
/// bare function name (e.g. `sys_tgkill`).
///
/// Add to this list — and explain why — only when a syscall's
/// mutation cannot meaningfully be cred-gated against another
/// subject.
const ALLOW_LIST: &[&str] = &[
    // tgid==caller.pid constraint makes source == target; cred check
    // is trivially permitted. Comment at the syscall site notes the
    // future migration point for cross-process tgkill.
    "sys_tgkill",
    // Hot path (cred_service_v_1 §"Hot path: use"): write reuses
    // the access grant minted at open(). The body also contains a
    // kernel-synthesised SIGPIPE self-send on broken-pipe writes,
    // which is not subject to cred::require_signal_send (the kernel
    // is sending to the caller itself on its own behalf, not on a
    // foreign request).
    "sys_write",
    // Hot path: ftruncate operates on an already-open fd; the open()
    // path minted the write grant and is the authorization
    // publication site. Re-checking cred at ftruncate would be the
    // cold-path "everything becomes a token" model the design
    // explicitly rejects (§"Not every operation is tokenized").
    "sys_ftruncate",
];

pub(crate) fn lint_invariants_cred_check(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut violations: Vec<String> = Vec::new();
    let mut audited_count: usize = 0;
    let mut allowlisted_seen: BTreeSet<&'static str> = BTreeSet::new();

    for file in &files {
        let rel = relative(root, file).replace('\\', "/");
        let fname = file.file_name().unwrap().to_str().unwrap_or("");
        if fname == "mod.rs" || fname == "numbers.rs" || fname == "tests.rs" {
            continue;
        }
        if rel.contains("/tests/") {
            continue;
        }

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let lines: Vec<&str> = text.lines().collect();

        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim();
            let is_decl = trimmed.starts_with("pub(super) fn sys_")
                || trimmed.starts_with("pub(super) async fn sys_")
                || trimmed.starts_with("pub fn sys_")
                || trimmed.starts_with("pub async fn sys_");
            if !is_decl || !trimmed.contains('(') {
                i += 1;
                continue;
            }

            // Extract the function name. Look for `sys_<name>(` or `sys_<name><`.
            let after_fn = trimmed
                .split_once("fn ")
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed);
            let fn_name: String = after_fn
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !fn_name.starts_with("sys_") {
                i += 1;
                continue;
            }

            // Skip if explicitly allow-listed.
            let allowlisted = ALLOW_LIST.iter().any(|&n| n == fn_name);

            // Extract the function body via brace tracking.
            let body = extract_body(&lines, i);

            let has_mutator = MUTATOR_SIGNALS.iter().any(|sig| body.contains(sig));
            if !has_mutator {
                // Reader / pure-query / non-cred-mutator — out of scope.
                i += 1;
                continue;
            }

            if allowlisted {
                allowlisted_seen.insert(
                    ALLOW_LIST
                        .iter()
                        .copied()
                        .find(|&n| n == fn_name)
                        .unwrap_or(""),
                );
                i += 1;
                continue;
            }

            audited_count += 1;
            let has_check = CRED_CHECK_SIGNALS.iter().any(|sig| body.contains(sig));
            if !has_check {
                violations.push(format!(
                    "{}:{} — {} drives a cred-relevant mutator without calling \
                     any cred::checks::require_* / authorize_* / script_kill_* / \
                     script_deliver_signal entry point in its body",
                    rel,
                    i + 1,
                    fn_name,
                ));
            }

            i += 1;
        }
    }

    println!("Invariants Lint — cred-check (every cred-mutator is gated)");
    println!("===========================================================");
    println!(
        "audited mutator syscall arms: {audited_count}, allow-listed: {}",
        allowlisted_seen.len()
    );
    if !allowlisted_seen.is_empty() {
        println!("  allow-listed (audit out-of-scope):");
        for n in &allowlisted_seen {
            println!("    {n}");
        }
    }
    let count = violations.len();
    if count == 0 {
        println!("violations: 0  ok");
        return Ok(());
    }
    println!("violations: {count}");
    for v in &violations {
        println!("  {v}");
    }
    Err(format!(
        "cred-check lint failed — {count} cred-relevant mutator syscall arm(s) \
         missing an authorization gate in body. Either route the mutation through \
         a cred::checks::require_* / authorize_* predicate (or a cred-checked \
         signal::script_* script), or add the function to ALLOW_LIST in \
         xtask/src/lint_invariants_cred_check.rs with a rationale comment."
    ))
}

/// Extract the function body starting at line index `decl_line`.
/// Tracks brace depth from the first `{` to the matching `}` and
/// returns the joined body text (newline-separated).
fn extract_body(lines: &[&str], decl_line: usize) -> String {
    let mut brace_depth: i32 = 0;
    let mut in_body = false;
    let mut out = String::new();
    for line in lines.iter().skip(decl_line) {
        if line.contains('{') {
            brace_depth += line.matches('{').count() as i32;
            in_body = true;
        }
        if line.contains('}') {
            brace_depth -= line.matches('}').count() as i32;
        }
        if in_body {
            out.push_str(line);
            out.push('\n');
        }
        if in_body && brace_depth <= 0 {
            break;
        }
    }
    out
}
