//! Lint rule for STEP_MODEL_v2 §1 / INVARIANTS_v5 EBR-7: StepOp wrappers
//! must not store an `&Guard` field.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` §1:
//!
//! > Each step acquires its own epoch guard on entry and releases it on
//! > return.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md` EBR-7 and ASYNC-1: epoch guards are
//! `!Send`/`!Sync`, may not be nested on the same CPU, and may not cross an
//! `.await`. A StepOp wrap that stores `pub guard: &'a Guard<'a>` is a
//! design-shape that:
//!
//!   1. forces the syscall handler to hold a guard around the call,
//!   2. nests when `step()` itself wants a fresh guard,
//!   3. blocks the wrapping future from being `Send`, breaking the reactor's
//!      `Send + 'static` submission contract (REACTOR_v0 §Submission).
//!
//! This lint scans `crates/tx-subsystems/`, `crates/tx-scripts/`, and
//! `crates/tx-shims/` for `pub guard: &'…' (…::)?Guard<'…>` field declarations
//! and fails CI if any reappear. The ratchet ceiling is `0`.

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: number of `pub guard: &Guard` fields tolerated. Per the
/// fix that landed alongside the contract-violation cleanup (see
/// `docs/progress/STATUS.md` 2026-05-18), the design-correct count is zero.
const MAX_GUARD_FIELDS: usize = 0;

pub(crate) fn lint_invariants_step_guard(root: &Path) -> Result<()> {
    let roots = [
        root.join("crates/tx-subsystems/src"),
        root.join("crates/tx-scripts/src"),
        root.join("crates/tx-shims/src"),
    ];

    // Match lines like:
    //   `    pub guard: &'a Guard<'a>,`
    //   `    pub guard: &'a crate::execution::Guard<'a>,`
    // Plain substring scan rather than a regex dependency: a field
    // declaration line that starts (after trim) with `pub guard:` and
    // contains `&'` and `Guard<'`.
    fn is_step_guard_field(line: &str) -> bool {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("pub guard:") {
            return false;
        }
        // Strip end-of-line comment so a `// ...` doesn't sneak past.
        let code = if let Some(pos) = trimmed.find("//") {
            &trimmed[..pos]
        } else {
            trimmed
        };
        code.contains("&'") && code.contains("Guard<'")
    }

    let mut violations: Vec<String> = Vec::new();

    for root_dir in roots.iter().filter(|p| p.exists()) {
        let files = collect_files(root_dir, &["rs"]).map_err(|e| e.to_string())?;
        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");
            for (line_num, line) in text.lines().enumerate() {
                if is_step_guard_field(line) {
                    violations.push(format!("{rel}:{} — {}", line_num + 1, line.trim()));
                }
            }
        }
    }

    let count = violations.len();
    println!("Invariants Lint — step-guard (STEP_MODEL_v2 §1 / EBR-7)");
    println!("=======================================================");
    let status = if count > MAX_GUARD_FIELDS {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "`pub guard: &Guard` fields in StepOp wraps: {:>4}  (ceiling {})  {}",
        count, MAX_GUARD_FIELDS, status
    );
    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_GUARD_FIELDS {
        return Err(format!(
            "step-guard ratchet regression — {count} StepOp wraps store `&Guard` \
             > ceiling {MAX_GUARD_FIELDS}. Each `step()` must acquire its own \
             epoch guard per STEP_MODEL_v2 §1. See `docs/progress/STATUS.md` \
             (2026-05-18) and `INVARIANTS_v5` EBR-7 / ASYNC-1 for the rationale."
        ));
    }
    Ok(())
}
