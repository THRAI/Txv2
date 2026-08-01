//! ext4 Tier 1 lifecycle G0 lints.

use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Finding {
    path: String,
    line: usize,
    message: &'static str,
    snippet: String,
}

pub(crate) fn lint_invariants_ext4_lifecycle_ownership(root: &Path) -> Result<()> {
    let findings = scan_ext4_lifecycle_ownership(root)?;
    report(
        "ext4-lifecycle-ownership",
        "raw PageSlot/ring cleanup outside ext4 lifecycle owners",
        &findings,
    )
}

pub(crate) fn lint_invariants_ext4_no_direct_home_write(root: &Path) -> Result<()> {
    let findings = scan_ext4_direct_home_write(root)?;
    report(
        "ext4-no-direct-home-write",
        "production tx-ext4 direct home writes outside explicit owners",
        &findings,
    )
}

pub(crate) fn lint_invariants_ext4_durability_flags(root: &Path) -> Result<()> {
    let findings = scan_ext4_durability_flags(root)?;
    report(
        "ext4-durability-flags",
        "production ext4 durability writes must use FUA or flush fallback",
        &findings,
    )
}

fn scan_ext4_lifecycle_ownership(root: &Path) -> Result<Vec<Finding>> {
    scan_rs(
        root,
        &["crates/tx-ext4/src", "crates/tx-subsystems/src"],
        |path, line| {
            if lifecycle_owner_allowed(path) {
                return None;
            }
            let code = code_before_comment(line)?;
            let patterns = [
                "release_journal_extent",
                "release_extent_token",
                "drop_journal_extent",
                "JournalExtentToken::release",
                "OwnedFileIoRequest::settle",
                "FileIoLifecycle::settle",
                "PageSlot::settle",
                "cleanup_pageslot",
            ];
            if patterns.iter().any(|pattern| code.contains(pattern)) {
                Some(
                    "lifecycle terminal cleanup must stay inside PageBacked, ext4 mutation, or mount settlement owners",
                )
            } else {
                None
            }
        },
    )
}

fn scan_ext4_direct_home_write(root: &Path) -> Result<Vec<Finding>> {
    scan_rs(root, &["crates/tx-ext4/src"], |path, line| {
        if direct_home_write_allowed(path) {
            return None;
        }
        let code = code_before_comment(line)?;
        let mutating_pager = [
            ".create_regular_file(",
            ".remove_dir_entry(",
            ".add_dir_entry(",
            ".write_page(",
            ".write_inode_meta_journaled(",
            ".set_inode_size(",
        ];
        if mutating_pager.iter().any(|pattern| code.contains(pattern)) {
            Some(
                "production direct pager mutation must be routed through the mutation journal owner",
            )
        } else {
            None
        }
    })
}

fn scan_ext4_durability_flags(root: &Path) -> Result<Vec<Finding>> {
    scan_rs(root, &["crates/tx-ext4/src"], |path, line| {
        if durability_write_allowed(path) {
            return None;
        }
        let code = code_before_comment(line)?;
        if code.contains(".write_block(") {
            Some(
                "durability-sensitive ext4 writes must be paired with FUA or an explicit barrier fallback",
            )
        } else {
            None
        }
    })
}

fn scan_rs(
    root: &Path,
    dirs: &[&str],
    classify: impl Fn(&str, &str) -> Option<&'static str>,
) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for dir in dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }
        let files = collect_files(&dir_path, &["rs"]).map_err(|err| err.to_string())?;
        for file in files {
            let rel = relative(root, &file).replace('\\', "/");
            if should_skip_file(&rel) {
                continue;
            }
            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            for (idx, line) in text.lines().enumerate() {
                let Some(message) = classify(&rel, line) else {
                    continue;
                };
                findings.push(Finding {
                    path: rel.clone(),
                    line: idx + 1,
                    message,
                    snippet: line.trim().chars().take(120).collect(),
                });
            }
        }
    }
    Ok(findings)
}

fn report(name: &str, title: &str, findings: &[Finding]) -> Result<()> {
    println!("Invariants Lint - {name}");
    println!("{}", "=".repeat(19 + name.len()));
    println!("{title}: {:>4}", findings.len());
    if !findings.is_empty() {
        println!();
        println!("  sites (first 40):");
        for finding in findings.iter().take(40) {
            println!(
                "  {}:{} - {}: {}",
                finding.path, finding.line, finding.message, finding.snippet
            );
        }
        if findings.len() > 40 {
            println!("  ... and {} more", findings.len() - 40);
        }
        return Err(format!("{name} found {} violation(s)", findings.len()));
    }
    Ok(())
}

fn should_skip_file(path: &str) -> bool {
    path.contains("/tests/")
        || path.ends_with("/tests.rs")
        || path.ends_with("_test.rs")
        || path.ends_with("tests_v3.rs")
}

fn lifecycle_owner_allowed(path: &str) -> bool {
    matches!(
        path,
        "crates/tx-subsystems/src/page_backed/lifecycle.rs"
            | "crates/tx-subsystems/src/page_backed/mod.rs"
            | "crates/tx-subsystems/src/mount/settlement.rs"
            | "crates/tx-ext4/src/mutation_lifecycle.rs"
            | "crates/tx-ext4/src/journal.rs"
            | "crates/tx-ext4/src/settlement.rs"
    )
}

fn direct_home_write_allowed(path: &str) -> bool {
    matches!(
        path,
        "crates/tx-ext4/src/mutation_lifecycle.rs"
            | "crates/tx-ext4/src/journal.rs"
            | "crates/tx-ext4/src/settlement.rs"
            | "crates/tx-ext4/src/read_backend.rs"
    )
}

fn durability_write_allowed(path: &str) -> bool {
    matches!(
        path,
        "crates/tx-ext4/src/journal.rs"
            | "crates/tx-ext4/src/host_async.rs"
            | "crates/tx-ext4/src/mutation_lifecycle.rs"
    )
}

fn code_before_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        return None;
    }
    Some(line.split_once("//").map(|(code, _)| code).unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::{
        code_before_comment, direct_home_write_allowed, lifecycle_owner_allowed,
        scan_ext4_direct_home_write,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ext4_direct_home_write_lint_rejects_namespace_callsite() {
        let root = temp_root();
        let path = root.join("crates/tx-ext4/src/new_namespace.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "fn create(pager: &mut Pager) { pager.create_regular_file(parent, name, mode, uid, gid, 0); }\n",
        )
        .unwrap();

        let findings = scan_ext4_direct_home_write(&root).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, "crates/tx-ext4/src/new_namespace.rs");
    }

    #[test]
    fn ext4_direct_home_write_lint_allows_owner_module() {
        let root = temp_root();
        let path = root.join("crates/tx-ext4/src/mutation_lifecycle.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "fn create(pager: &mut Pager) { pager.create_regular_file(parent, name, mode, uid, gid, 0); }\n",
        )
        .unwrap();

        let findings = scan_ext4_direct_home_write(&root).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn ext4_owner_allowlist_is_precise() {
        assert!(lifecycle_owner_allowed(
            "crates/tx-ext4/src/mutation_lifecycle.rs"
        ));
        assert!(!lifecycle_owner_allowed("crates/tx-ext4/src/namespace.rs"));
        assert!(direct_home_write_allowed(
            "crates/tx-ext4/src/mutation_lifecycle.rs"
        ));
        assert!(!direct_home_write_allowed(
            "crates/tx-ext4/src/namespace.rs"
        ));
        assert!(!direct_home_write_allowed("crates/tx-ext4/src/pager.rs"));
        assert!(!direct_home_write_allowed(
            "crates/tx-ext4/src/new_namespace.rs"
        ));
    }

    #[test]
    fn comments_are_ignored() {
        assert!(code_before_comment("// pager.create_regular_file(").is_none());
        assert_eq!(
            code_before_comment("pager.write_page(); // comment"),
            Some("pager.write_page(); ")
        );
    }

    fn temp_root() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tx-ext4-lint-test-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
