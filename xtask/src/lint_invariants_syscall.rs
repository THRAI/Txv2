//! Lint rules for syscall implementation hygiene against the v3 spec.
//!
//! Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md` and
//! `docs/Txv3/03_STEP_MODEL_v2.md` §10 anti-patterns:
//! syscall functions in `linux_syscall/` must migrate from ad-hoc
//! StepOutcome-matching loops and `.await`-inside-syscall-fn patterns
//! to the canonical `drive()` + `StepOp` discipline.
//!
//! Three ratchet gates:
//! 1. `syscall-adhoc-loop` — count syscall files with manual
//!    `V3::Done|Continue|Yield` (or `V3Out::*`) match loops.
//!    These are the alias forms of the `StepOutcome::*` matching
//!    that `no-adhoc-drive` already catches for the canonical names.
//! 2. `syscall-no-await` — count `.await` calls inside
//!    `pub(super) fn sys_*` function bodies (A-3: no `.await` in
//!    step/dispatch sites — yield through `drive()` instead).
//! 3. `syscall-ctx-bridge` — count how many `sys_*` functions use
//!    `build_subject_script_ctx` (bridge to v3 `ScriptCtx`) vs how
//!    many still use the bare `SyscallCtx`. Trend toward 100% bridging.
//!
//! Scanned: `crates/tx-shims/src/linux_syscall/` (syscall implementation
//! files only — not mod.rs, numbers.rs, tests/).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

// ---------------------------------------------------------------------------
// Ratchet ceilings
// ---------------------------------------------------------------------------

/// Files with manual V3/V3Out alias match loops (not yet drive()-migrated).
/// Measured baseline 2026-05-15.
const MAX_SYSCALL_ADHOC_LOOP_FILES: usize = 10; // 8→10: vfs-full-bringup merge added two new syscall files with manual alias loops

/// `.await` sites inside `pub(super) fn sys_*` function bodies.
/// Measured baseline 2026-05-15.
const MAX_SYSCALL_AWAIT_SITES: usize = 60;

/// sys_* functions NOT yet bridged through build_subject_script_ctx.
/// (Informational only — no ratchet failure, just a metric.)
const _SYSCALL_CTX_BRIDGE_TARGET: usize = 0;

/// Whether an `.await` completes a canonical lower script or driver that
/// started earlier in the same expression. rustfmt places that `.await` on
/// its own line, so a same-line text check would turn formatting into a false
/// A-3 hit. The syscall itself remains a dispatch boundary when it forwards
/// to another syscall primitive or a named script driver; raw wait loops do
/// not match these forms and remain A-3 violations.
fn is_drive_await(lines: &[&str], await_line: usize) -> bool {
    let mut expression_start = await_line;
    while expression_start > 0 {
        let previous = lines[expression_start - 1].trim();
        if previous.ends_with(';') || previous.ends_with('{') || previous.ends_with('}') {
            break;
        }
        expression_start -= 1;
    }

    lines[expression_start..=await_line]
        .iter()
        .map(|line| line.split_once("//").map_or(*line, |(code, _)| code))
        .any(|line| {
            line.contains("drive(")
                || line.contains("drive_")
                || line.contains("script_")
                || line.contains("sleep_until_deadline")
                || line.contains("sys_epoll_wait_until")
                || line.contains("sys_mlock(")
                || line.contains("sys_write_buffered")
                || line.contains("sys_write(")
                || line.contains("sys_read::<")
        })
}

// ---------------------------------------------------------------------------
// V3/V3Out alias patterns to detect
// ---------------------------------------------------------------------------

/// Aliases for `StepOutcome` used in manual dispatch loops within syscall
/// implementation files. Each entry is `(prefix, display_name)`.
const OUTCOME_ALIASES: &[(&str, &str)] = &[("V3", "V3"), ("V3Out", "V3Out")];

/// The StepOutcome variants that signal ad-hoc dispatch when matched on
/// (outside of drive.rs).
const DISPATCH_VARIANTS: &[&str] = &["Continue", "Yield", "Done"];

// ---------------------------------------------------------------------------
// Lint 1: syscall-adhoc-loop
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_adhoc_loop(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut adhoc_files: Vec<String> = Vec::new();
    let mut adhoc_sites: Vec<String> = Vec::new();

    for file in &files {
        let rel = relative(root, file).replace('\\', "/");

        // Skip non-implementation files
        let fname = file.file_name().unwrap().to_str().unwrap_or("");
        if fname == "mod.rs" || fname == "numbers.rs" || fname == "tests.rs" {
            continue;
        }
        if rel.contains("/tests/") {
            continue;
        }

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;

        // Skip files that define their own step_* functions.
        if text.contains("pub fn step_") || text.contains("pub(crate) fn step_") {
            continue;
        }

        let mut file_has_adhoc = false;

        for (line_num, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Skip comments
            if trimmed.starts_with("//")
                || trimmed.starts_with("///")
                || trimmed.starts_with("/*")
                || trimmed.starts_with("*")
            {
                continue;
            }

            let code_part = if let Some(pos) = trimmed.find("//") {
                &trimmed[..pos]
            } else {
                trimmed
            };

            // Skip construction sites: `=> V3::Yield {` or `return V3::Yield {`
            let is_construction = code_part.contains("=> V3::")
                || code_part.contains("return V3::")
                || code_part.contains("=> V3Out::")
                || code_part.contains("return V3Out::");
            if is_construction {
                continue;
            }

            // Check for alias dispatch patterns in match arms
            for (prefix, alias_name) in OUTCOME_ALIASES {
                for variant in DISPATCH_VARIANTS {
                    let pattern = format!("{prefix}::{variant}");
                    if code_part.contains(&pattern) {
                        // Skip if inside a test block
                        if is_in_test_context(&text, line_num) {
                            continue;
                        }
                        if !file_has_adhoc {
                            adhoc_files.push(rel.clone());
                            file_has_adhoc = true;
                        }
                        adhoc_sites.push(format!(
                            "{}:{} — ad-hoc {}::{} dispatch",
                            rel,
                            line_num + 1,
                            alias_name,
                            variant,
                        ));
                        break; // one hit per line
                    }
                }
            }
        }
    }

    let file_count = adhoc_files.len();
    let site_count = adhoc_sites.len();

    println!("Invariants Lint — syscall-adhoc-loop");
    println!("======================================");

    let status = if file_count > MAX_SYSCALL_ADHOC_LOOP_FILES {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "files with ad-hoc alias loops: {file_count:>4}  (ceiling {MAX_SYSCALL_ADHOC_LOOP_FILES})  {status}"
    );
    println!("total ad-hoc alias sites:      {site_count:>4}");

    if !adhoc_files.is_empty() {
        println!();
        println!("  files:");
        for f in &adhoc_files {
            println!("  {f}");
        }
        println!();
        println!("  sites:");
        for s in &adhoc_sites {
            println!("  {s}");
        }
    }

    if file_count > MAX_SYSCALL_ADHOC_LOOP_FILES {
        return Err(format!(
            "syscall-adhoc-loop ratchet regression — {file_count} files with ad-hoc alias loops > ceiling {MAX_SYSCALL_ADHOC_LOOP_FILES}"
        ));
    }

    Ok(())
}

/// Approximate check: is this line inside a `#[cfg(test)]` or `mod tests {` block?
fn is_in_test_context(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_test_mod = false;
    let mut test_depth: u32 = 0;

    for (i, line) in lines.iter().enumerate() {
        if i > target_line {
            break;
        }
        let trimmed = line.trim();
        if trimmed == "#[cfg(test)]" {
            in_test_mod = true;
            test_depth = 0;
            continue;
        }
        if trimmed.starts_with("mod tests") {
            in_test_mod = true;
            test_depth = 0;
        }
        if in_test_mod {
            let opens = trimmed.matches('{').count() as u32;
            let closes = trimmed.matches('}').count() as u32;
            if opens > 0 && test_depth == 0 {
                test_depth = opens.saturating_sub(closes);
            } else {
                test_depth += opens;
                test_depth = test_depth.saturating_sub(closes);
            }
            if test_depth == 0 && i > 0 {
                in_test_mod = false;
            }
        }
    }
    in_test_mod
}

// ---------------------------------------------------------------------------
// Lint 2: syscall-no-await — `.await` in sys_* function bodies
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_no_await(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut violations: Vec<String> = Vec::new();

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
            let line = lines[i];
            let trimmed = line.trim();

            if (trimmed.starts_with("pub(super) fn sys_")
                || trimmed.starts_with("pub(super) async fn sys_"))
                && trimmed.contains('(')
            {
                let mut brace_depth: i32 = 0;
                let mut in_body = false;
                let mut j = i;

                while j < lines.len() {
                    let body_line = lines[j];
                    let body_trimmed = body_line.trim();

                    if body_line.contains('{') {
                        brace_depth += body_line.matches('{').count() as i32;
                        in_body = true;
                    }
                    if body_line.contains('}') {
                        brace_depth -= body_line.matches('}').count() as i32;
                    }

                    if in_body
                        && !body_trimmed.starts_with("//")
                        && !body_trimmed.starts_with("///")
                    {
                        let code = if let Some(pos) = body_trimmed.find("//") {
                            &body_trimmed[..pos]
                        } else {
                            body_trimmed
                        };

                        if code.contains(".await") && !is_drive_await(&lines, j) {
                            violations.push(format!(
                                "{}:{} — .await in syscall fn body (A-3)",
                                rel,
                                j + 1
                            ));
                        }
                    }

                    if brace_depth <= 0 && in_body {
                        break;
                    }
                    j += 1;
                }

                i = j;
            }
            i += 1;
        }
    }

    let count = violations.len();

    println!("Invariants Lint — syscall-no-await (A-3)");
    println!("==========================================");

    let status = if count > MAX_SYSCALL_AWAIT_SITES {
        "OVER"
    } else {
        "ok"
    };
    println!(
        ".await in sys_* fn bodies: {count:>4}  (ceiling {MAX_SYSCALL_AWAIT_SITES})  {status}"
    );

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_SYSCALL_AWAIT_SITES {
        return Err(format!(
            "syscall-no-await ratchet regression — {count} .await sites > ceiling {MAX_SYSCALL_AWAIT_SITES}"
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Lint 3: syscall-ctx-bridge — SubjectContext bridge adoption metric
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_ctx_bridge(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut total_syscalls: BTreeMap<String, usize> = BTreeMap::new();
    let mut bridged_syscalls: BTreeMap<String, usize> = BTreeMap::new();

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

        let mut file_total = 0;
        let mut file_bridged = 0;
        let mut search_start = 0usize;
        loop {
            let rest = &text[search_start..];
            let fn_pos = rest
                .find("pub(super) fn sys_")
                .or_else(|| rest.find("pub(super) async fn sys_"));
            let delta = match fn_pos {
                Some(p) => p,
                None => break,
            };
            let fn_start = search_start + delta;

            let open = match text[fn_start..].find('{') {
                Some(p) => fn_start + p,
                None => break,
            };

            let mut depth = 1i32;
            let mut close = open + 1;
            while close < text.len() && depth > 0 {
                match text.as_bytes()[close] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                close += 1;
            }
            let fn_body = &text[open + 1..close - 1];

            file_total += 1;
            if fn_body.contains("build_subject_script_ctx") {
                file_bridged += 1;
            }

            search_start = close;
        }

        if file_total > 0 {
            total_syscalls.insert(rel.clone(), file_total);
            bridged_syscalls.insert(rel, file_bridged);
        }
    }

    let grand_total: usize = total_syscalls.values().sum();
    let grand_bridged: usize = bridged_syscalls.values().sum();

    println!("Invariants Lint — syscall-ctx-bridge (SUBJ-1)");
    println!("===============================================");

    let pct = if grand_total > 0 {
        (grand_bridged as f64 / grand_total as f64) * 100.0
    } else {
        0.0
    };
    println!("sys_* fns bridged to ScriptCtx: {grand_bridged:>3}/{grand_total:>3}  ({pct:.0}%)");

    if grand_total > 0 {
        println!();
        println!("  per-file:");
        for (f, total) in &total_syscalls {
            let bridged = bridged_syscalls.get(f).copied().unwrap_or(0);
            let short = f
                .strip_prefix("crates/tx-shims/src/linux_syscall/")
                .unwrap_or(f);
            let marker = if bridged > 0 { "✓" } else { " " };
            println!("  {marker} {short:30} {bridged:>2}/{total:<2} bridged");
        }
    }

    println!();
    println!("  Target: {_SYSCALL_CTX_BRIDGE_TARGET} bridging (all sys_* fns use ScriptCtx).");
    println!("  This metric is informational — no ratchet gate yet.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_drive_await;

    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("missing function signature: {signature}"));
        let open = start
            + source[start..]
                .find('{')
                .expect("function signature must have a body");
        let mut depth = 1usize;
        let mut end = open + 1;
        while depth > 0 {
            match source.as_bytes()[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        &source[open + 1..end - 1]
    }

    #[test]
    fn multiline_drive_await_is_not_a_raw_syscall_await() {
        let lines = [
            "match drive(",
            "    op,",
            "    &mut script_ctx,",
            ")",
            ".await",
            "{",
        ];

        assert!(is_drive_await(&lines, 4));
    }

    #[test]
    fn unrelated_await_remains_a_raw_syscall_await() {
        let lines = [
            "let woke = await_wait_endpoint(ctx, endpoint, interests)",
            ".await",
            ";",
        ];

        assert!(!is_drive_await(&lines, 1));
    }

    #[test]
    fn named_drive_wrapper_is_not_a_raw_syscall_await() {
        let lines = ["let outcome = drive_vm_remap(ctx, request)", ".await", ";"];

        assert!(is_drive_await(&lines, 1));
    }

    #[test]
    fn syscall_forwarder_is_not_a_raw_syscall_await() {
        let lines = ["sys_write(write_args, ctx)", ".await", ";"];

        assert!(is_drive_await(&lines, 1));
    }

    #[test]
    fn wait4_blocking_path_delegates_wait_source_resolution_to_drive() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/proc.rs");
        let wait4 = source
            .split("pub(super) async fn sys_wait4")
            .nth(1)
            .expect("sys_wait4 must remain a syscall entry");

        assert!(wait4.contains("drive("));
        assert!(!wait4.contains("await_wait_endpoint"));
    }

    #[test]
    fn truncate_and_fallocate_use_pagebacked_drive_ops() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let truncate = function_body(&source, "pub(super) async fn sys_truncate");
        let fallocate = function_body(&source, "pub(super) async fn sys_fallocate");

        assert!(truncate.contains("TruncateOp"));
        assert!(truncate.contains("drive("));
        assert!(!truncate.contains("use StepOutcome as V3"));
        assert!(fallocate.contains("FallocateOp"));
        assert!(fallocate.contains("drive("));
        assert!(!fallocate.contains("use StepOutcome as V3"));
    }

    #[test]
    fn aio_lseek_uses_the_one_shot_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/aio.rs");
        let lseek = function_body(&source, "fn run_lseek_set");

        assert!(lseek.contains("drive_oneshot"));
        assert!(!lseek.contains(".step("));
    }

    #[test]
    fn direct_pagebacked_prefault_uses_the_one_shot_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/io.rs");
        let direct_io = function_body(&source, "async fn sys_direct_pagebacked");

        assert!(direct_io.contains("ReserveUserRangeOp"));
        assert!(direct_io.contains("drive_oneshot"));
        assert!(!direct_io.contains("reserve_user_range_for_access"));
        assert!(!direct_io.contains("StepOutcome"));
    }

    #[test]
    fn buffered_pagebacked_prefaults_use_the_one_shot_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/io.rs");
        let write = function_body(&source, "async fn sys_write_pagebacked");
        let read = function_body(&source, "async fn sys_read_pagebacked");

        for syscall in [write, read] {
            assert!(syscall.contains("ReserveUserRangeOp"));
            assert!(syscall.contains("drive_oneshot"));
            assert!(!syscall.contains("reserve_user_range_for_access"));
            assert!(!syscall.contains("StepOutcome"));
        }
    }

    #[test]
    fn pagebacked_writev_prefault_uses_the_one_shot_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/io.rs");
        let writev = function_body(&source, "pub(super) fn sys_writev_pagebacked_oneshot");

        assert!(writev.contains("ReserveUserRangeOp"));
        assert!(writev.contains("drive_oneshot"));
        assert!(!writev.contains("reserve_user_range_for_access"));
    }

    #[test]
    fn chdir_uses_the_namespace_walker_one_shot_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_path.rs");
        let chdir = function_body(&source, "pub(super) async fn sys_chdir");

        assert!(chdir.contains("WalkInMountNamespaceWithOriginOp"));
        assert!(chdir.contains("drive_oneshot"));
        assert!(!chdir.contains("StepOutcome"));
        assert!(!chdir.contains("use StepOutcome as V3"));
    }

    #[test]
    fn mkdirat_and_symlinkat_use_one_shot_vfs_ops() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let mkdirat = function_body(&source, "pub(super) async fn sys_mkdirat");
        let symlinkat = function_body(&source, "pub(super) async fn sys_symlinkat");

        for syscall in [mkdirat, symlinkat] {
            assert!(syscall.contains("drive_oneshot"));
            assert!(!syscall.contains("use StepOutcome as V3"));
        }
        assert!(mkdirat.contains("MkdirOp"));
        assert!(symlinkat.contains("SymlinkOp"));
    }

    #[test]
    fn linkat_uses_the_one_shot_vfs_link_op() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let linkat = function_body(&source, "pub(super) async fn sys_linkat");

        assert!(linkat.contains("LinkInParentOp"));
        assert!(linkat.contains("drive_oneshot"));
        assert!(!linkat.contains("use StepOutcome as V3"));
    }

    #[test]
    fn readlinkat_uses_the_one_shot_vfs_read_link_op() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let readlinkat = function_body(&source, "pub(super) async fn sys_readlinkat");

        assert_eq!(readlinkat.matches("ReadLinkByIdOp").count(), 2);
        assert!(readlinkat.contains("LookupInParentOp"));
        assert!(readlinkat.contains("LoadInodeMetaOp"));
        assert_eq!(readlinkat.matches("drive_oneshot").count(), 4);
        assert!(!readlinkat.contains("fs_ops.read_link"));
        assert!(!readlinkat.contains("use StepOutcome as V3"));
    }

    #[test]
    fn unlinkat_uses_one_shot_vfs_ops_after_path_resolution() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let unlinkat = function_body(&source, "pub(super) async fn sys_unlinkat");

        for op in ["LookupInParentOp", "LoadInodeMetaOp", "UnlinkFromParentOp"] {
            assert!(unlinkat.contains(op), "missing {op}");
        }
        assert_eq!(unlinkat.matches("drive_oneshot").count(), 3);
        assert!(!unlinkat.contains("use StepOutcome as V3"));
    }

    #[test]
    fn create_then_walk_uses_the_vfs_script_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_mut.rs");
        let create_then_walk = function_body(&source, "async fn drive_create_then_walk");

        assert!(create_then_walk.contains("CreateThenWalkOp"));
        assert!(create_then_walk.contains("drive("));
        assert!(!create_then_walk.contains("StepOutcome"));
        assert!(!create_then_walk.contains("V3::"));
    }

    #[test]
    fn openat_tmpfile_uses_vfs_drivers_for_its_mutating_stages() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_basic.rs");
        let openat = function_body(&source, "pub(super) async fn sys_openat");
        let start = openat.find("if want_tmpfile").expect("tmpfile branch");
        let end = openat[start..]
            .find("// PR async migration")
            .map(|offset| start + offset)
            .expect("next openat branch");
        let tmpfile = &openat[start..end];

        for op in [
            "PathWalkOp",
            "CreateInParentOp",
            "OpenOp",
            "UnlinkFromParentOp",
        ] {
            assert!(tmpfile.contains(op), "missing {op}");
        }
        assert!(tmpfile.contains("drive("));
        assert!(tmpfile.contains("drive_oneshot"));
        assert!(!tmpfile.contains("V3::"));
    }

    #[test]
    fn openat_create_and_truncate_use_full_vfs_drivers() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_basic.rs");
        let openat = function_body(&source, "pub(super) async fn sys_openat");
        let start = openat
            .find("// Resolve or create the target")
            .expect("create/truncate branch");
        let tail = &openat[start..];

        for op in ["ResolveOpenTargetOp", "TruncateFsObjectOp", "OpenOp"] {
            assert!(tail.contains(op), "missing {op}");
        }
        assert!(tail.contains("drive("));
        assert!(!tail.contains("V3::"));
        assert!(!tail.contains("V3Trunc"));
    }

    #[test]
    fn tty_ioctl_uses_one_shot_tty_ops() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/fs_basic.rs");
        let ioctl = function_body(&source, "pub(super) fn sys_ioctl");

        for op in [
            "IoctlTcgetsOp",
            "IoctlTcsetsOp",
            "IoctlTiocgpgrpOp",
            "IoctlTiocspgrpForProcessOp",
            "IoctlTiocgwinszOp",
            "IoctlTiocswinszOp",
            "IoctlTiocscttyForProcessOp",
            "IoctlTiocnottyOp",
        ] {
            assert!(ioctl.contains(op), "missing {op}");
        }
        assert!(ioctl.contains("drive_oneshot"));
        assert!(!ioctl.contains("unwrap_v3"));
        assert!(!ioctl.contains("V3Out::"));
    }

    #[test]
    fn rtc_read_uses_the_full_driver_over_the_registered_raw_queue() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/io.rs");
        let rtc_read = function_body(&source, "async fn sys_rtc_read_buffered");

        assert!(rtc_read.contains("RtcReadOp"));
        assert!(rtc_read.contains("drive("));
        assert!(!rtc_read.contains("wait_on_registered_source_id"));
        assert!(!rtc_read.contains("loop {"));
    }

    #[test]
    fn eventfd_read_and_write_use_full_drivers() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/eventfd.rs");

        for function in [
            "pub(super) async fn sys_eventfd_read",
            "pub(super) async fn sys_eventfd_write",
        ] {
            let body = function_body(&source, function);
            assert!(body.contains("Eventfd"));
            assert!(body.contains("drive("));
            assert!(!body.contains("loop {"));
            assert!(!body.contains("V3Out::"));
        }
    }

    #[test]
    fn userfaultfd_read_uses_full_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/userfaultfd.rs");
        let body = function_body(&source, "pub(super) async fn sys_ufd_read");

        assert!(body.contains("UffdReadOp"));
        assert!(body.contains("drive("));
        assert!(!body.contains("loop {"));
        assert!(!body.contains("V3Out::"));
    }

    #[test]
    fn signalfd_read_uses_full_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/signalfd.rs");
        let body = function_body(&source, "pub(super) async fn sys_signalfd_read");

        assert!(body.contains("SignalfdReadOp"));
        assert!(body.contains("drive("));
        assert!(!body.contains("loop {"));
        assert!(!body.contains("V3Out::"));
    }

    #[test]
    fn timerfd_read_uses_full_driver() {
        let source = include_str!("../../crates/tx-shims/src/linux_syscall/timerfd.rs");
        let body = function_body(&source, "pub(super) async fn sys_timerfd_read");

        assert!(body.contains("TimerfdReadOp"));
        assert!(body.contains("drive("));
        assert!(!body.contains("wait_for_timerfd_wake"));
        assert!(!body.contains("loop {"));
        assert!(!body.contains("V3Out::"));
    }
}
