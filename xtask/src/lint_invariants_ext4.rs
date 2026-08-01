//! Lint rules for ext4 Tier 1 production cutover.
//!
//! These are static ratchets for the Task 14 G0 boundary:
//! - lifecycle ownership must reach the discovered-journal mount path;
//! - production ext4 code must not fall back to raw home-block writes; and
//! - durability-capability plumbing must preserve explicit flush/FUA policy.

use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

const EXT4_PROD_FILES: &[&str] = &[
    "crates/tx-ext4/src/read_backend.rs",
    "crates/tx-ext4/src/pager.rs",
    "crates/tx-ext4/src/namespace.rs",
    "crates/tx-ext4/src/mount.rs",
    "crates/tx-kernel/src/init.rs",
    "crates/tx-subsystems/src/device.rs",
    "crates/tx-drivers/src/virtio/mmio.rs",
    "crates/tx-drivers/src/virtio/blk.rs",
];

pub(crate) fn lint_invariants_ext4_lifecycle_ownership(root: &Path) -> Result<()> {
    let mut violations = Vec::new();
    let init = root.join("crates/tx-kernel/src/init.rs");
    let text = fs::read_to_string(&init).map_err(|err| format!("{}: {err}", init.display()))?;
    let rel = relative(root, &init).replace('\\', "/");

    for banned in [
        "mount_ext4_read_write(",
        "mount_ext4_read_write_with_backend_planner(",
        "mount_ext4_read_write_with_journal_io_manager_planner(",
        "mount_ext4_read_write_with_mutation_journal_io_manager_planner(",
    ] {
        if text.contains(banned) {
            violations.push(format!("{rel} — legacy production constructor: {banned}"));
        }
    }

    if !text.contains("mount_ext4_read_write_with_discovered_journal(") {
        violations.push(format!(
            "{rel} — missing discovered-journal production mount call"
        ));
    }

    println!("Invariants Lint — ext4-lifecycle-ownership");
    println!("==========================================");
    for violation in &violations {
        println!("  {violation}");
    }
    if violations.is_empty() {
        println!("  ok");
        Ok(())
    } else {
        Err(format!(
            "ext4 lifecycle ownership regression — {} violation(s)",
            violations.len()
        ))
    }
}

pub(crate) fn lint_invariants_ext4_no_direct_home_write(root: &Path) -> Result<()> {
    let mut violations = Vec::new();
    for rel_path in EXT4_PROD_FILES {
        let path = root.join(rel_path);
        if !path.exists() {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        for (line_num, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//")
                || trimmed.starts_with("///")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
            {
                continue;
            }
            let code = trimmed.split("//").next().unwrap_or(trimmed);
            if code.contains(".write_block(")
                || code.contains(".write_blocks(")
                || code.contains("write_home")
                || code.contains("home write")
            {
                violations.push(format!(
                    "{}:{} — direct home write: {}",
                    rel_path,
                    line_num + 1,
                    code.trim()
                ));
            }
        }
    }

    println!("Invariants Lint — ext4-no-direct-home-write");
    println!("============================================");
    for violation in &violations {
        println!("  {violation}");
    }
    if violations.is_empty() {
        println!("  ok");
        Ok(())
    } else {
        Err(format!(
            "ext4 direct home-write regression — {} violation(s)",
            violations.len()
        ))
    }
}

pub(crate) fn lint_invariants_ext4_durability_flags(root: &Path) -> Result<()> {
    let mut violations = Vec::new();
    for rel_path in [
        "crates/tx-subsystems/src/device.rs",
        "crates/tx-drivers/src/virtio/mmio.rs",
        "crates/tx-drivers/src/virtio/blk.rs",
        "crates/tx-ext4/src/journal.rs",
    ] {
        let path = root.join(rel_path);
        if !path.exists() {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let has_flush = text.contains("flush: true");
        let has_fua_false = text.contains("fua: false");
        let has_fua_guard = text.contains("if options.fua") || text.contains("BlockFlags::FUA");
        if rel_path.ends_with("device.rs") {
            if !(text.contains("if options.fua && !self.reg.ops.durability_capabilities().fua")
                && text.contains("if !self.reg.ops.durability_capabilities().flush"))
            {
                violations.push(format!(
                    "{rel_path} — missing FUA/flush admission guards in block handle"
                ));
            }
        } else if !(has_flush && has_fua_false && has_fua_guard) {
            violations.push(format!(
                "{rel_path} — durability capability or FUA trace missing"
            ));
        }
    }

    println!("Invariants Lint — ext4-durability-flags");
    println!("=======================================");
    for violation in &violations {
        println!("  {violation}");
    }
    if violations.is_empty() {
        println!("  ok");
        Ok(())
    } else {
        Err(format!(
            "ext4 durability-flag regression — {} violation(s)",
            violations.len()
        ))
    }
}

#[allow(dead_code)]
pub(crate) fn lint_ext4_production_roots(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for rel_path in EXT4_PROD_FILES {
        let path = root.join(rel_path);
        if path.exists() {
            out.push(relative(root, &path));
        }
    }
    let _ = collect_files(root, &["rs"]).map_err(|err| err.to_string())?;
    Ok(out)
}
