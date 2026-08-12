//! Lint rule: legacy wait-channel usage ratchet.
//!
//! The v3 wake path is object-owned `Arc<WaitSource>` plus mailbox
//! subscriptions. `crates/tx-subsystems/src/wait_source.rs` remains the
//! compatibility bridge for older `WaitToken` -> reactor `Channel` awaits.
//! New code should not add direct production uses of that compatibility API.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: production references to the legacy wait-channel bridge.
///
/// Baseline captured 2026-05-24 while adding the lint. Drive this toward zero
/// as syscall/script wait drivers migrate to `WaitSource::prepare(..)`.
const MAX_LEGACY_WAIT_CHANNEL_SITES: usize = 0;

const LEGACY_WAIT_APIS: &[&str] = &[
    "wait_on_token",
    "lookup_wait_channel",
    "register_wait_channel",
    "release_wait_channel",
];

const CHANNEL_RETIREMENT_TERMS: &[&str] = &[
    "Channel",
    "Mask",
    "WaitFuture",
    "fire_legacy_channel",
    "fire_legacy",
    "register_wait_channel",
    "register_wait_channel_with_id",
    "lookup_wait_channel",
    "release_wait_channel",
    "RegisteredWaitFuture",
    "RegisteredWaitSource",
];

const FD_READY_FACADE_TERMS: &[&str] = &[
    "socket_poll_mask_from_file",
    "socket_poll_wait_token_from_file",
    "pselect_socket_read_ready",
    "pselect_socket_write_ready",
    "pselect_socket_blocked_interests",
    "timerfd_readable_level",
    "EVENTFD_POLL_READABLE",
    "EVENTFD_POLL_WRITABLE",
    "TIMERFD_POLL_READABLE",
];

#[derive(Debug, PartialEq, Eq)]
struct RetirementFinding {
    rel: String,
    line: usize,
    term: &'static str,
    snippet: String,
}

pub(crate) fn lint_invariants_legacy_wait_channel(root: &Path) -> Result<()> {
    let target_dirs = [
        "crates/tx-kernel",
        "crates/tx-shims",
        "crates/tx-scripts",
        "crates/tx-subsystems",
    ];

    let mut sites = Vec::new();
    let mut by_api: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut retirement_findings = Vec::new();

    for dir in target_dirs {
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
            retirement_findings.extend(lint_channel_retirement_text(&rel, &text));
            retirement_findings.extend(lint_fd_ready_facade_text(&rel, &text));
            for (line_idx, line) in text.lines().enumerate() {
                let Some(code) = code_before_comment(line) else {
                    continue;
                };
                if is_in_test_context(&text, line_idx) {
                    continue;
                }

                for api in LEGACY_WAIT_APIS {
                    if contains_ident(code, api) {
                        *by_api.entry(api).or_insert(0) += 1;
                        sites.push(format!(
                            "{rel}:{} - legacy wait-channel API `{api}`: {}",
                            line_idx + 1,
                            code.trim().chars().take(100).collect::<String>()
                        ));
                    }
                }
            }
        }
    }

    let count = sites.len();
    let over = count > MAX_LEGACY_WAIT_CHANNEL_SITES;

    println!("Invariants Lint - legacy-wait-channel");
    println!("=======================================");
    println!(
        "legacy wait-channel sites: {:>4}  (ceiling {})  {}",
        count,
        MAX_LEGACY_WAIT_CHANNEL_SITES,
        if over { "OVER" } else { "ok" }
    );

    if !by_api.is_empty() {
        println!();
        println!("  by API:");
        for api in LEGACY_WAIT_APIS {
            if let Some(n) = by_api.get(api) {
                println!("  {api:<22} {n:>4}");
            }
        }
    }

    if !sites.is_empty() {
        println!();
        println!("  sites (first 40):");
        for site in sites.iter().take(40) {
            println!("  {site}");
        }
        if sites.len() > 40 {
            println!("  ... and {} more", sites.len() - 40);
        }
    }

    print_channel_retirement_report(&retirement_findings);

    if over {
        return Err(format!(
            "legacy-wait-channel ratchet regression - {count} production legacy wait-channel API sites > ceiling {MAX_LEGACY_WAIT_CHANNEL_SITES}. Prefer object-owned Arc<WaitSource> registration/notification and migrate syscall drivers away from wait_source::wait_on_token."
        ));
    }

    Ok(())
}

fn lint_fd_ready_facade_text(rel: &str, text: &str) -> Vec<RetirementFinding> {
    if rel != "crates/tx-shims/src/linux_syscall/io.rs" {
        return Vec::new();
    }

    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let Some(code) = code_before_comment(line) else {
            continue;
        };
        if is_in_test_context(text, line_idx) {
            continue;
        }
        for term in FD_READY_FACADE_TERMS {
            if contains_ident(code, term) {
                findings.push(RetirementFinding {
                    rel: rel.to_string(),
                    line: line_idx + 1,
                    term,
                    snippet: code.trim().chars().take(120).collect(),
                });
            }
        }
    }
    findings
}

fn lint_channel_retirement_text(rel: &str, text: &str) -> Vec<RetirementFinding> {
    if !is_channel_retirement_scan_path(rel) {
        return Vec::new();
    }

    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let Some(code) = code_before_comment(line) else {
            continue;
        };
        if is_in_test_context(text, line_idx) {
            continue;
        }
        for term in CHANNEL_RETIREMENT_TERMS {
            if contains_ident(code, term) {
                findings.push(RetirementFinding {
                    rel: rel.to_string(),
                    line: line_idx + 1,
                    term,
                    snippet: code.trim().chars().take(120).collect(),
                });
            }
        }
    }
    findings
}

fn is_channel_retirement_scan_path(rel: &str) -> bool {
    (rel.starts_with("crates/tx-reactor/src/")
        || rel.starts_with("crates/tx-subsystems/src/")
        || rel.starts_with("crates/tx-shims/src/"))
        && rel.ends_with(".rs")
}

fn print_channel_retirement_report(findings: &[RetirementFinding]) {
    let mut by_term: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut by_path: BTreeMap<&str, usize> = BTreeMap::new();
    for finding in findings {
        *by_term.entry(finding.term).or_insert(0) += 1;
        *by_path.entry(finding.rel.as_str()).or_insert(0) += 1;
    }

    println!();
    println!("Channel retirement inventory");
    println!("============================");
    println!(
        "channel-retirement findings: {:>4}  (report-only)",
        findings.len()
    );

    if !by_term.is_empty() {
        println!();
        println!("  by term:");
        for term in CHANNEL_RETIREMENT_TERMS {
            if let Some(count) = by_term.get(term) {
                println!("  {term:<32} {count:>4}");
            }
        }
    }

    if !by_path.is_empty() {
        println!();
        println!("  top files:");
        for (path, count) in by_path.iter().rev().take(20) {
            println!("  {path:<72} {count:>4}");
        }
    }

    if !findings.is_empty() {
        println!();
        println!("  sites (first 80):");
        for finding in findings.iter().take(80) {
            println!(
                "  {}:{} - `{}`: {}",
                finding.rel, finding.line, finding.term, finding.snippet
            );
        }
        if findings.len() > 80 {
            println!("  ... and {} more", findings.len() - 80);
        }
    }
}

fn should_skip_file(rel: &str) -> bool {
    rel == "crates/tx-subsystems/src/wait_source.rs"
        || rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
}

#[cfg(test)]
mod tests {
    use super::{lint_channel_retirement_text, RetirementFinding};

    #[test]
    fn channel_retirement_flags_channel_and_registry_terms() {
        let findings = lint_channel_retirement_text(
            "crates/tx-subsystems/src/pipe/notification.rs",
            r#"
use crate::adapter::wait_routing::{Channel, WaitFuture};
fn install(channel: Channel) {
    crate::wait_source::register_wait_channel_with_id(7, channel.clone());
    wait_routing::fire_legacy_channel(&channel, 1);
}
"#,
        );

        assert!(has_term(&findings, "Channel"));
        assert!(has_term(&findings, "WaitFuture"));
        assert!(has_term(&findings, "register_wait_channel_with_id"));
        assert!(has_term(&findings, "fire_legacy_channel"));
    }

    #[test]
    fn channel_retirement_ignores_comments() {
        let findings = lint_channel_retirement_text(
            "crates/tx-subsystems/src/pipe/mod.rs",
            r#"
// Channel should not count in comments.
/// WaitFuture should not count in docs.
fn endpoint() {}
"#,
        );

        assert!(findings.is_empty());
    }

    #[test]
    fn fd_ready_facade_inventory_flags_io_poll_readiness_terms() {
        let findings = super::lint_fd_ready_facade_text(
            "crates/tx-shims/src/linux_syscall/io.rs",
            r#"
fn scan() {
    let _ = socket_poll_mask_from_file(&file, &guard);
    let _ = socket_poll_wait_token_from_file(&file, mask, &guard);
    let _ = timerfd_readable_level::<P>(tfd);
}
"#,
        );

        assert!(has_term(&findings, "socket_poll_mask_from_file"));
        assert!(has_term(&findings, "socket_poll_wait_token_from_file"));
        assert!(has_term(&findings, "timerfd_readable_level"));
    }

    fn has_term(findings: &[RetirementFinding], term: &str) -> bool {
        findings.iter().any(|finding| finding.term == term)
    }
}

fn code_before_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
    {
        return None;
    }
    Some(line.split_once("//").map(|(code, _)| code).unwrap_or(line))
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

fn is_in_test_context(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_test_mod = false;
    let mut test_depth = 0u32;

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

#[cfg(test)]
mod tests {
    use super::{lint_channel_retirement_text, RetirementFinding};

    #[test]
    fn channel_retirement_flags_channel_and_registry_terms() {
        let findings = lint_channel_retirement_text(
            "crates/tx-subsystems/src/pipe/notification.rs",
            r#"
use crate::adapter::wait_routing::{Channel, WaitFuture};
fn install(channel: Channel) {
    crate::wait_source::register_wait_channel_with_id(7, channel.clone());
    wait_routing::fire_legacy_channel(&channel, 1);
}
"#,
        );

        assert!(has_term(&findings, "Channel"));
        assert!(has_term(&findings, "WaitFuture"));
        assert!(has_term(&findings, "register_wait_channel_with_id"));
        assert!(has_term(&findings, "fire_legacy_channel"));
    }

    #[test]
    fn channel_retirement_ignores_comments() {
        let findings = lint_channel_retirement_text(
            "crates/tx-subsystems/src/pipe/mod.rs",
            r#"
// Channel should not count in comments.
/// WaitFuture should not count in docs.
fn endpoint() {}
"#,
        );

        assert!(findings.is_empty());
    }

    #[test]
    fn fd_ready_facade_inventory_flags_io_poll_readiness_terms() {
        let findings = super::lint_fd_ready_facade_text(
            "crates/tx-shims/src/linux_syscall/io.rs",
            r#"
fn scan() {
    let _ = socket_poll_mask_from_file(&file, &guard);
    let _ = socket_poll_wait_token_from_file(&file, mask, &guard);
    let _ = timerfd_readable_level::<P>(tfd);
}
"#,
        );

        assert!(has_term(&findings, "socket_poll_mask_from_file"));
        assert!(has_term(&findings, "socket_poll_wait_token_from_file"));
        assert!(has_term(&findings, "timerfd_readable_level"));
    }

    fn has_term(findings: &[RetirementFinding], term: &str) -> bool {
        findings.iter().any(|finding| finding.term == term)
    }
}
