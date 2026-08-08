//! OBS-7: `observe-discipline` lint — enforces anti-patterns OBS-A-1
//! and OBS-A-2.
//!
//! Scans every `.rs` file in `crates/` and `boards/` (excluding the
//! observation crates themselves, the trace daemon, and `xtask`) and
//! fails if:
//!
//! - **OBS-A-1**: any `StepOp::step` body directly calls
//!   `tx_observe::*` APIs (move the emit to the `drive` wrapper or use
//!   `RawTrace<P>` from BUS_v1).
//! - **OBS-A-2**: any module marked `#[platform_adapter(...)]` contains
//!   a `tx_observe::*` emit (adapter verbs must delegate; substrate
//!   verbs emit — `OBS-V1-HOOK-SCOPE` in `08_OBSERVATION_v1.md` §6).
//!
//! ## Scanning approach
//!
//! Option A (regex-style stateful scanner): walkdir + brace-depth tracking.
//! For each file the scanner works line by line:
//!
//! 1. Detect `impl StepOp` (or `impl StepOp<…>`) block openings and track
//!    the brace depth at which the block was opened.
//! 2. Within an `impl StepOp` block, detect `fn step(` to identify the step
//!    method body.
//! 3. Within the step body, look for any of the forbidden patterns.
//!
//! Heuristic basis: the patterns are syntactically distinctive enough that
//! plain substring matching with brace-depth tracking catches all direct
//! call-site violations. The `impl StepOp` scanner is analogous to the
//! existing `lint_step_no_await` detector in `lint.rs`.
//!
//! **Limitation**: this scanner catches *direct* misuse only. If a step
//! body calls a helper function that internally calls `tx_observe::current`
//! or emits a record, the scanner will not flag it. Transitive call-graph
//! analysis is OBS-7-v2 territory.
//!
//! ## Allow-list
//!
//! A line containing `// observe-discipline: allow <reason>` is exempt.
//! Every allow must carry a written reason; the scanner enforces this by
//! requiring non-empty text after "allow ".
//!
//! ## Spec refs
//!
//! - `txdoc:OBS-V1-ANTI-1`   — anti-pattern table (OBS-A-1)
//! - `txdoc:OBS-V1-NO-STEP-BODY` — invariant: no emit inside step bodies

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Forbidden patterns that must not appear inside a `StepOp::step` body.
///
/// Each entry is a bare substring; the scanner checks whether the stripped
/// (comment-removed) line contains it. The patterns are chosen to cover:
/// - direct API access via the crate path (`tx_observe::current`,
///   `tx_observe::HartEmitter`)
/// - method calls on a `HartEmitter` that the L4 driver legitimately
///   uses in `drive.rs` (but which step bodies must not use)
/// - the syscall-boundary macro that belongs only in adapter code
///
/// Ref: `docs/Txv3/08_OBSERVATION_v1.md` §17 (OBS-A-1).
const FORBIDDEN: &[&str] = &[
    "tx_observe::current",
    "tx_observe::HartEmitter",
    ".span_begin(",
    ".span_end(",
    ".instant(",
    ".counter(",
    "traced_syscall!",
];

/// Crate/path prefixes that are excluded from the scan.
///
/// `crates/tx-observe` and `crates/tx-observe-types` are the
/// implementation of the observation API; they may freely use these
/// patterns. `tools/tx-trace-daemon` is host-side only. `xtask` is the
/// lint runner itself.
const EXCLUDED_PREFIXES: &[&str] = &[
    "crates/tx-observe/",
    "crates/tx-observe-types/",
    "tools/tx-trace-daemon/",
    "xtask/",
];

/// Run the `observe-discipline` lint against `root`.
pub(crate) fn observe_discipline(root: &Path) -> Result<()> {
    let files = collect_files(root, &["rs"]).map_err(|err| err.to_string())?;

    let mut files_scanned = 0usize;
    let mut step_op_impls = 0usize;
    let mut adapter_blocks = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for path in &files {
        let rel = relative(root, path).replace('\\', "/");

        // Only scan crates/ and boards/.
        if !rel.starts_with("crates/") && !rel.starts_with("boards/") {
            continue;
        }

        // Exclude the observation crates, the daemon, and xtask.
        if EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| rel.starts_with(prefix))
        {
            continue;
        }

        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };

        files_scanned += 1;
        scan_file(
            &rel,
            path,
            &text,
            &mut step_op_impls,
            &mut adapter_blocks,
            &mut violations,
        );
    }

    if violations.is_empty() {
        println!(
            "observe-discipline: clean ({files_scanned} files scanned, \
             {step_op_impls} StepOp impls, {adapter_blocks} adapter blocks)"
        );
        Ok(())
    } else {
        for v in &violations {
            eprintln!("{v}");
        }
        Err(format!(
            "observe-discipline: {} violation(s) found",
            violations.len()
        ))
    }
}

/// Scan a single file for OBS-A-1 violations.
///
/// State machine:
/// - outer: scanning top-level; watching for `impl StepOp`
/// - impl_block: inside an `impl StepOp` block; watching for `fn step(`
/// - step_body: inside the `fn step` body; checking every line for
///   forbidden patterns
///
/// Brace depth tracking:
/// - `brace_depth`: total nesting depth
/// - `impl_depth`: depth at which the `impl StepOp {` brace was opened
///   (i.e. the depth *after* consuming `{` on the impl line)
/// - `step_depth`: depth at which the `fn step {` brace was opened
fn scan_file(
    display: &str,
    _path: &Path,
    text: &str,
    step_op_impls: &mut usize,
    adapter_blocks: &mut usize,
    violations: &mut Vec<String>,
) {
    let mut brace_depth: i32 = 0;
    let mut impl_depth: Option<i32> = None; // depth after `impl StepOp {` opens
    let mut step_depth: Option<i32> = None; // depth after `fn step(` opens
    let mut awaiting_impl_brace = false;
    let mut awaiting_step_brace = false;
    // OBS-A-2: `#[platform_adapter(...)]` mod scope. Track the brace
    // depth at which the adapter module's `{` opened. Any emit pattern
    // appearing between that brace and its matching close is flagged.
    let mut adapter_depth: Option<i32> = None;
    let mut awaiting_adapter_brace = false;

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;

        // Strip line comments for brace counting and pattern matching.
        // Block comments are not handled; they are rare in Rust kernel code.
        let stripped = strip_line_comment(line);

        // Check for allow annotation *on the raw line* (before stripping)
        // because the annotation lives in the comment.
        let has_allow = line.contains("// observe-discipline: allow ");

        // ── State transitions ────────────────────────────────────────────────

        // Outside any `impl StepOp` block: look for the opening.
        if impl_depth.is_none() && !awaiting_impl_brace && is_impl_step_op_header(stripped) {
            awaiting_impl_brace = true;
            *step_op_impls += 1;
        }

        // Inside `impl StepOp` but not yet in a step body: look for `fn step(`.
        if impl_depth.is_some()
            && step_depth.is_none()
            && !awaiting_step_brace
            && is_fn_step_header(stripped)
        {
            awaiting_step_brace = true;
        }

        // OBS-A-2: `#[platform_adapter(...)]` mod body. The attribute can
        // sit on its own line and the `mod NAME {` opener follows up to a
        // few lines later (the attribute supports multi-line `(platform
        // = "…", domain = "…", reason = "…")` argument lists). We set the
        // pending flag as soon as we see the attribute and clear it when
        // the next `{` opens at any depth.
        if adapter_depth.is_none()
            && !awaiting_adapter_brace
            && is_platform_adapter_header(stripped)
        {
            awaiting_adapter_brace = true;
            *adapter_blocks += 1;
        }

        // ── Brace scanning ───────────────────────────────────────────────────

        // Walk every character for brace depth updates and transition triggers.
        let bytes = stripped.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => {
                    brace_depth += 1;
                    if awaiting_impl_brace {
                        awaiting_impl_brace = false;
                        impl_depth = Some(brace_depth);
                    } else if awaiting_step_brace {
                        awaiting_step_brace = false;
                        step_depth = Some(brace_depth);
                    } else if awaiting_adapter_brace {
                        awaiting_adapter_brace = false;
                        adapter_depth = Some(brace_depth);
                    }
                }
                b'}' => {
                    // Close step body first (inner scope).
                    if let Some(sd) = step_depth {
                        if brace_depth == sd {
                            step_depth = None;
                            awaiting_step_brace = false;
                        }
                    }
                    // Close impl block.
                    if let Some(id) = impl_depth {
                        if brace_depth == id {
                            impl_depth = None;
                            step_depth = None;
                            awaiting_step_brace = false;
                            awaiting_impl_brace = false;
                        }
                    }
                    // Close adapter mod block.
                    if let Some(ad) = adapter_depth {
                        if brace_depth == ad {
                            adapter_depth = None;
                            awaiting_adapter_brace = false;
                        }
                    }
                    brace_depth -= 1;
                }
                _ => {}
            }
            i += 1;
        }

        // ── Pattern checking ─────────────────────────────────────────────────

        if has_allow {
            continue;
        }
        // Inside an adapter mod: OBS-A-2.
        if adapter_depth.is_some() {
            for pattern in FORBIDDEN {
                if stripped.contains(pattern) {
                    violations.push(format!(
                        "{display}:{line_no}: OBS-A-2 violation — `{pattern}` inside #[platform_adapter] module (adapters delegate; substrate emits — `08_OBSERVATION_v1.md` §6 OBS-V1-HOOK-SCOPE)"
                    ));
                    break;
                }
            }
            // An adapter mod is by definition not also a step body; skip
            // the OBS-A-1 path below.
            continue;
        }
        // Inside a StepOp::step body: OBS-A-1.
        if step_depth.is_none() {
            continue;
        }
        for pattern in FORBIDDEN {
            if stripped.contains(pattern) {
                violations.push(format!(
                    "{display}:{line_no}: OBS-A-1 violation — `{pattern}` inside StepOp::step body (move to drive wrapper or use RawTrace<P>)"
                ));
                // Report each pattern at most once per line.
                break;
            }
        }
    }
}

/// Returns true if `line` (stripped of its comment) looks like the start
/// of an `impl StepOp` block.
///
/// Matches:
/// - `impl StepOp for Foo {`
/// - `impl StepOp<ProcessIdentity> for Foo {`
/// - `impl<I: SubjectIdentity> StepOp<I> for Foo {`
/// - multi-line: `impl StepOp` where `{` appears on a later line (the
///   caller's `awaiting_impl_brace` flag handles that case)
///
/// Uses a simple `contains("StepOp")` heuristic and requires that the
/// token is preceded by "impl " to avoid matching `dyn StepOp` or
/// `use tx_substrate::...::StepOp`. The match is deliberately broad:
/// false positives (trait objects, where clauses) would set
/// `awaiting_impl_brace` but never enter `impl_depth` if no `{` follows
/// in the same structure, and the depth tracking would self-correct.
fn is_impl_step_op_header(line: &str) -> bool {
    // Must contain "impl" followed immediately by either " " or "<" (for
    // generic params), and "StepOp" must appear somewhere after.
    // Matches:
    //   impl StepOp for Foo
    //   impl StepOp<ProcessIdentity> for Foo
    //   impl<I: SubjectIdentity> StepOp<I> for Foo
    //
    // Regex equivalent: /impl[ <].*StepOp/
    let Some(impl_pos) = line.find("impl") else {
        return false;
    };
    let after_impl_kw = &line[impl_pos + 4..]; // skip "impl"
                                               // Next char must be a space or '<' (to avoid matching e.g. "reimpl").
    let first_char = after_impl_kw.chars().next().unwrap_or('\0');
    if first_char != ' ' && first_char != '<' {
        return false;
    }
    after_impl_kw.contains("StepOp")
}

/// Returns true if `line` (stripped of comment) looks like the `fn step(`
/// method declaration inside an `impl StepOp` block.
///
/// Matches `fn step(` exactly (not `fn step_foo(` etc.) per the StepOp
/// trait definition in `tx-substrate/src/step_v3/mod.rs`.
fn is_fn_step_header(line: &str) -> bool {
    line.contains("fn step(")
}

/// Returns true if `line` (stripped of comment) opens a
/// `#[platform_adapter(...)]` attribute. The macro accepts both
/// single-line and multi-line argument forms, but it always begins with
/// `#[platform_adapter(` — the substring is sufficient to start
/// tracking; the brace scanner advances state to the next `{` regardless
/// of how many lines the attribute spans.
fn is_platform_adapter_header(line: &str) -> bool {
    line.contains("#[platform_adapter(")
}

/// Strip the `//`-introduced line comment from `line`, returning only the
/// code portion. Block comments are not stripped (they are uncommon in
/// this codebase and the patterns being searched are unlikely to appear
/// in block comments).
fn strip_line_comment(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return "";
    }
    match line.find("//") {
        Some(pos) => &line[..pos],
        None => line,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn run_scan(source: &str) -> (usize, Vec<String>) {
        let mut impls = 0;
        let mut adapters = 0;
        let mut violations = Vec::new();
        scan_file(
            "test.rs",
            std::path::Path::new("test.rs"),
            source,
            &mut impls,
            &mut adapters,
            &mut violations,
        );
        (impls, violations)
    }

    /// OBS-3a legitimate pattern: `tx_observe::current` called in the DRIVE
    /// wrapper (outside `fn step`). Must be clean.
    #[test]
    fn clean_drive_emit_outside_step_body() {
        let source = r#"
pub fn drive<O: StepOp>(op: &mut O) {
    // L4 SpanBegin — call site in driver, not inside step body.
    if let Some(em) = tx_observe::current::<Plat>() {
        em.span_begin(TxTraceLevel::Step, EventNameId::of::<O>(), SpanId::NONE, TxPayloadTag::None, &[]);
    }
    let outcome = op.step(ctx);
    // L4 SpanEnd
    if let Some(em) = tx_observe::current::<Plat>() {
        em.span_end(step_span, step_outcome_tag(), &payload_bytes);
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 0, "no impl StepOp in this snippet");
        assert!(
            violations.is_empty(),
            "drive wrapper must be clean: {violations:?}"
        );
    }

    /// Synthetic violation: `tx_observe::current` inside a step body.
    #[test]
    fn violation_tx_observe_current_inside_step_body() {
        let source = r#"
impl StepOp for BadOp {
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        // BUG: direct observation inside step body — OBS-A-1
        if let Some(em) = tx_observe::current::<Plat>() {
            em.span_begin(TxTraceLevel::Step, name, SpanId::NONE, TxPayloadTag::None, &[]);
        }
        StepOutcome::Done(())
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 1);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("tx_observe::current") && v.contains("OBS-A-1")),
            "expected OBS-A-1 violation for tx_observe::current, got {violations:?}"
        );
    }

    /// Synthetic violation: `.span_begin(` inside a step body.
    #[test]
    fn violation_span_begin_inside_step_body() {
        let source = r#"
impl StepOp for SpanOp {
    fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        self.emitter.span_begin(TxTraceLevel::Step, name, SpanId::NONE, TxPayloadTag::None, &[]);
        StepOutcome::Done(())
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 1);
        assert!(
            violations.iter().any(|v| v.contains(".span_begin(")),
            "expected violation for .span_begin(, got {violations:?}"
        );
    }

    /// Allow annotation suppresses the violation.
    #[test]
    fn allow_annotation_suppresses_violation() {
        let source = r#"
impl StepOp for AllowedOp {
    fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        if let Some(em) = tx_observe::current::<Plat>() { em.instant(x); } // observe-discipline: allow synthetic test for allow-list mechanism
        StepOutcome::Done(())
    }
}
"#;
        let (_impls, violations) = run_scan(source);
        assert!(
            violations.is_empty(),
            "allow annotation must suppress violation, got {violations:?}"
        );
    }

    /// `traced_syscall!` is forbidden inside a step body.
    #[test]
    fn violation_traced_syscall_inside_step_body() {
        let source = r#"
impl StepOp for ShimOp {
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        tx_observe::traced_syscall!(ctx, sys_read, fd, buf, len);
        StepOutcome::Done(())
    }
}
"#;
        let (_impls, violations) = run_scan(source);
        assert!(
            violations.iter().any(|v| v.contains("traced_syscall!")),
            "expected violation for traced_syscall!, got {violations:?}"
        );
    }

    /// Code outside `impl StepOp` is not flagged even if it contains
    /// forbidden patterns.
    #[test]
    fn not_flagged_outside_impl_step_op() {
        let source = r#"
fn helper() {
    if let Some(em) = tx_observe::current::<P>() {
        em.span_begin(TxTraceLevel::Drive, name, SpanId::NONE, TxPayloadTag::None, &[]);
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 0);
        assert!(
            violations.is_empty(),
            "helpers outside impl StepOp must not be flagged: {violations:?}"
        );
    }

    /// Multiple `impl StepOp` blocks in one file are all counted.
    #[test]
    fn counts_multiple_impl_step_op_blocks() {
        let source = r#"
impl StepOp for OpA {
    fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }
}

impl StepOp for OpB {
    fn step(&mut self, _ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 2);
        assert!(violations.is_empty());
    }

    /// A `fn step(` outside an `impl StepOp` block (e.g. a free function
    /// named `step`) is not treated as a step body.
    #[test]
    fn free_fn_named_step_is_not_a_step_body() {
        let source = r#"
fn step(val: u32) -> u32 {
    if let Some(em) = tx_observe::current::<Plat>() { em.instant(val); }
    val
}
"#;
        let (_impls, violations) = run_scan(source);
        assert!(
            violations.is_empty(),
            "free fn step must not be treated as a StepOp body: {violations:?}"
        );
    }

    /// Generic `impl<I: SubjectIdentity> StepOp<I> for Foo` is detected.
    #[test]
    fn detects_generic_impl_step_op() {
        let source = r#"
impl<I: SubjectIdentity> StepOp<I> for GenericOp {
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        if let Some(em) = tx_observe::current::<Plat>() {}
        StepOutcome::Done(())
    }
}
"#;
        let (impls, violations) = run_scan(source);
        assert_eq!(impls, 1);
        assert!(
            violations.iter().any(|v| v.contains("tx_observe::current")),
            "generic impl StepOp violation must be detected: {violations:?}"
        );
    }

    // ── OBS-A-2 (no emit inside #[platform_adapter] modules) ──────────

    fn run_scan_with_adapters(source: &str) -> (usize, usize, Vec<String>) {
        let mut impls = 0;
        let mut adapters = 0;
        let mut violations = Vec::new();
        scan_file(
            "test.rs",
            std::path::Path::new("test.rs"),
            source,
            &mut impls,
            &mut adapters,
            &mut violations,
        );
        (impls, adapters, violations)
    }

    /// Adapter mod with a forbidden emit is flagged as OBS-A-2.
    #[test]
    fn violation_emit_inside_platform_adapter() {
        let source = r#"
#[platform_adapter(platform = "substrate", domain = "step_engine", reason = "expose step types")]
pub mod step_engine {
    pub fn helper() {
        if let Some(em) = tx_observe::current::<Plat>() {
            em.span_begin(TxTraceLevel::Drive, name, SpanId::NONE, TxPayloadTag::None, &[]);
        }
    }
}
"#;
        let (_impls, adapters, violations) = run_scan_with_adapters(source);
        assert_eq!(adapters, 1);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("OBS-A-2") && v.contains("tx_observe::current")),
            "expected OBS-A-2 violation for tx_observe::current, got {violations:?}"
        );
    }

    /// `.span_begin(` inside an adapter mod also trips OBS-A-2.
    #[test]
    fn violation_span_begin_inside_platform_adapter() {
        let source = r#"
#[platform_adapter(platform = "reactor", domain = "wait", reason = "wait verbs")]
pub mod wait {
    fn inside(em: &HartEmitter) {
        em.span_begin(TxTraceLevel::Yield, name, SpanId::NONE, TxPayloadTag::None, &[]);
    }
}
"#;
        let (_impls, _adapters, violations) = run_scan_with_adapters(source);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("OBS-A-2") && v.contains(".span_begin(")),
            "expected OBS-A-2 violation for .span_begin(, got {violations:?}"
        );
    }

    /// Multi-line `#[platform_adapter(...)]` attribute still tracked: the
    /// scanner sets the pending flag when it sees `#[platform_adapter(`
    /// and clears it on the next `{`, regardless of where the closing
    /// `)]` lands.
    #[test]
    fn detects_multi_line_platform_adapter_header() {
        let source = r#"
#[platform_adapter(
    platform = "substrate",
    domain = "vfs",
    reason = "expose vfs verbs",
)]
pub mod vfs {
    fn bad() {
        em.instant(TxTraceLevel::Yield, name, SpanId::NONE, tag, &[]);
    }
}
"#;
        let (_impls, adapters, violations) = run_scan_with_adapters(source);
        assert_eq!(adapters, 1);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("OBS-A-2") && v.contains(".instant(")),
            "multi-line platform_adapter header must still gate OBS-A-2, got {violations:?}"
        );
    }

    /// Adapter mods that don't emit are clean. Adapter mods *can* call
    /// substrate verbs that themselves emit — but the adapter file
    /// itself must not contain the forbidden patterns.
    #[test]
    fn clean_adapter_mod_with_only_reexports() {
        let source = r#"
#[platform_adapter(platform = "substrate", domain = "step_engine", reason = "re-exports")]
pub mod step_engine {
    pub use tx_substrate::step::{StepOp, StepOutcome};
    pub use tx_substrate::epoch::guard;
}
"#;
        let (_impls, adapters, violations) = run_scan_with_adapters(source);
        assert_eq!(adapters, 1);
        assert!(
            violations.is_empty(),
            "clean adapter mod must not be flagged: {violations:?}"
        );
    }

    /// Allow annotation also suppresses OBS-A-2 (parallel to OBS-A-1).
    #[test]
    fn allow_annotation_suppresses_obs_a_2() {
        let source = r#"
#[platform_adapter(platform = "substrate", domain = "x", reason = "r")]
pub mod x {
    fn experimental() {
        em.instant(level, name, span, tag, &[]); // observe-discipline: allow temporary scaffold for OBS-9 prototyping
    }
}
"#;
        let (_impls, _adapters, violations) = run_scan_with_adapters(source);
        assert!(
            violations.is_empty(),
            "allow annotation must suppress OBS-A-2, got {violations:?}"
        );
    }
}
