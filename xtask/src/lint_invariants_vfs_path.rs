//! Lint rule: syscall VFS path resolution must route through the
//! dirfd-aware resolver facade.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

const FORBIDDEN_CODE_PATTERNS: &[&str] = &[
    "step_walk",
    "walk_to_completion",
    "walk_from",
    "walk_from_process",
];

const STALE_COMMENT_PATTERNS: &[&str] = &[
    "dirfd == AT_FDCWD only",
    "AT_FDCWD only",
    "only supports `dirfd == AT_FDCWD`",
];

pub(crate) fn lint_invariants_vfs_path_interface(root: &Path) -> Result<()> {
    let syscalls = root.join("crates/tx-shims/src/linux_syscall");
    if !syscalls.exists() {
        return Ok(());
    }

    let mut sites = Vec::new();
    let mut by_pattern: BTreeMap<&'static str, usize> = BTreeMap::new();
    let files = collect_files(&syscalls, &["rs"]).map_err(|err| err.to_string())?;
    for file in files {
        let rel = relative(root, &file).replace('\\', "/");
        if should_skip_file(&rel) {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", rel))?;
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if is_comment(trimmed) {
                for pattern in STALE_COMMENT_PATTERNS {
                    if trimmed.contains(pattern) {
                        *by_pattern.entry(pattern).or_insert(0) += 1;
                        sites.push(format!(
                            "{rel}:{} - stale dirfd-only comment `{pattern}`: {}",
                            line_idx + 1,
                            trimmed.chars().take(110).collect::<String>()
                        ));
                    }
                }
                continue;
            }

            let code = line.split_once("//").map(|(code, _)| code).unwrap_or(line);
            for pattern in FORBIDDEN_CODE_PATTERNS {
                if contains_ident(code, pattern) {
                    *by_pattern.entry(pattern).or_insert(0) += 1;
                    sites.push(format!(
                        "{rel}:{} - direct VFS walker interface `{pattern}`: {}",
                        line_idx + 1,
                        code.trim().chars().take(110).collect::<String>()
                    ));
                }
            }
        }
    }

    println!("Invariants Lint - vfs-path-interface");
    println!("=====================================");
    println!(
        "direct syscall walker/stale dirfd-only sites: {:>4}  {}",
        sites.len(),
        if sites.is_empty() { "ok" } else { "OVER" }
    );
    if !by_pattern.is_empty() {
        println!();
        println!("  by pattern:");
        for (pattern, count) in by_pattern {
            println!("  {pattern:<42} {count:>4}");
        }
    }
    if !sites.is_empty() {
        println!();
        println!("  sites:");
        for site in sites.iter().take(80) {
            println!("  {site}");
        }
        return Err(format!(
            "vfs-path-interface violation - {} syscall sites bypass the dirfd-aware resolver facade",
            sites.len()
        ));
    }
    Ok(())
}

fn should_skip_file(rel: &str) -> bool {
    rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
        || rel.ends_with("crates/tx-shims/src/linux_syscall/fs_path.rs")
        || rel.ends_with("crates/tx-shims/src/linux_syscall/exec_op.rs")
}

fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
}

fn contains_ident(code: &str, ident: &str) -> bool {
    let bytes = code.as_bytes();
    let needle = ident.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    for start in 0..=bytes.len() - needle.len() {
        if &bytes[start..start + needle.len()] != needle {
            continue;
        }
        let before = start.checked_sub(1).and_then(|i| bytes.get(i).copied());
        let after = bytes.get(start + needle.len()).copied();
        if !is_ident_byte(before) && !is_ident_byte(after) {
            return true;
        }
    }
    false
}

fn is_ident_byte(byte: Option<u8>) -> bool {
    matches!(byte, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}
