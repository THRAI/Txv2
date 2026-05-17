//! Lint rule for SUBJ-1: SubjectContext must be threaded through step functions.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md` SUBJ-1: "Every script frame has exactly
//! one SubjectContext." SUBJ-4: "Both halves use typed StepOps identically."
//! This lint checks that `impl StepOp` blocks do not ignore `ScriptCtx` (i.e.,
//! do not use `_ctx` underscore prefix).

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: number of `_ctx: &mut ScriptCtx` occurrences.
/// Will be set to measured baseline after first run.
const MAX_IGNORED_SCRIPTCTX: usize = 109; // 79→80: InodeStatOp VFS StepOp wrap

pub(crate) fn lint_invariants_subject_context(root: &Path) -> Result<()> {
    let mut ignored: Vec<String> = Vec::new();

    let target_dirs = [
        "crates/tx-subsystems/src",
        "crates/tx-shims/src",
    ];

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");

            for (line_num, line) in text.lines().enumerate() {
                // Primary pattern: `_ctx: &mut ScriptCtx` in StepOp::step signature
                if line.contains("_ctx") && line.contains("ScriptCtx") {
                    ignored.push(format!(
                        "{rel}:{} — _ctx: &mut ScriptCtx (SUBJ-1: SubjectContext ignored)",
                        line_num + 1
                    ));
                }

                // Also catch: free functions that take `_ctx` parameter
                if line.contains("fn step_") && line.contains("_ctx") && line.contains("ScriptCtx") {
                    ignored.push(format!(
                        "{rel}:{} — step_* fn with _ctx: ScriptCtx (SUBJ-1)",
                        line_num + 1
                    ));
                }
            }
        }
    }

    let count = ignored.len();

    println!("Invariants Lint — subject-context (SUBJ-1)");
    println!("===========================================");

    let status = if count > MAX_IGNORED_SCRIPTCTX { "OVER" } else { "ok" };
    println!(
        "_ctx: &mut ScriptCtx occurrences: {:>4}  (ceiling {})  {}",
        count, MAX_IGNORED_SCRIPTCTX, status
    );

    for entry in &ignored {
        println!("  {entry}");
    }

    if count > MAX_IGNORED_SCRIPTCTX {
        return Err(format!(
            "subject-context ratchet regression — {count} ignored ScriptCtx > ceiling {MAX_IGNORED_SCRIPTCTX}"
        ));
    }

    Ok(())
}
