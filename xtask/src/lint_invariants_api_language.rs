//! Report-only lint for Phase 1 API-language convergence.
//!
//! This inventories adapter mechanism vocabulary and raw wait/readiness terms
//! while subsystem adapters are still converging on role-shaped exports.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

const ADAPTER_BUCKET: &str = "adapter mechanism language";
const WAIT_BUCKET: &str = "wait readiness raw language";

const ADAPTER_FORBIDDEN_TERMS: &[&str] = &[
    "ZonePolicy",
    "PayloadPolicy",
    "RetainedEntityPolicy",
    "CapProducingPolicy",
    "CoLocatedEntity",
    "IsPayloadPolicy",
    "ObserverNodePolicy",
    "PayloadBinding",
    "IdentitySlot",
    "OperationalRefExt",
    "RawPort",
    "RawQueue",
    "Channel",
    "Mask",
];

const WAIT_RAW_TERMS: &[&str] = &[
    "WaitToken",
    "WaitSourceId",
    "source_id",
    "InterestMask",
    "Channel",
    "Mask",
    "RawPort",
    "RawQueue",
];

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    rel: String,
    line: usize,
    bucket: &'static str,
    term: &'static str,
    snippet: String,
}

pub(crate) fn lint_invariants_api_language(root: &Path) -> Result<()> {
    let mut findings = Vec::new();

    for root_rel in ["crates/tx-subsystems/src", "crates/tx-shims/src"] {
        let path = root.join(root_rel);
        if !path.exists() {
            continue;
        }
        for file in collect_files(&path, &["rs"]).map_err(|err| err.to_string())? {
            let rel = relative(root, &file).replace('\\', "/");
            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            findings.extend(lint_api_language_text(&rel, &text));
        }
    }

    print_api_language_report(&findings);
    Ok(())
}

fn lint_api_language_text(rel: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let scan_adapter = is_adapter_scan_path(rel);
    let scan_wait = is_wait_raw_scan_path(rel);

    if !scan_adapter && !scan_wait {
        return findings;
    }

    for (line_idx, line) in text.lines().enumerate() {
        let Some(code) = code_before_comment(line) else {
            continue;
        };

        if scan_adapter {
            findings.extend(
                ADAPTER_FORBIDDEN_TERMS
                    .iter()
                    .copied()
                    .filter(|term| contains_ident(code, term))
                    .map(|term| finding(rel, line_idx + 1, ADAPTER_BUCKET, term, code)),
            );
        }

        if scan_wait {
            findings.extend(
                WAIT_RAW_TERMS
                    .iter()
                    .copied()
                    .filter(|term| contains_ident(code, term))
                    .map(|term| finding(rel, line_idx + 1, WAIT_BUCKET, term, code)),
            );
        }
    }

    findings
}

fn print_api_language_report(findings: &[Finding]) {
    let mut by_bucket: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut by_term: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    for finding in findings {
        *by_bucket.entry(finding.bucket).or_insert(0) += 1;
        *by_term.entry((finding.bucket, finding.term)).or_insert(0) += 1;
    }

    println!("Invariants Lint - api-language");
    println!("===============================");
    println!(
        "txdoc limited-language/api-language findings: {:>4}  (report-only)",
        findings.len()
    );

    if !by_bucket.is_empty() {
        println!();
        println!("  by bucket:");
        for (bucket, count) in &by_bucket {
            println!("  {bucket:<36} {count:>4}");
        }
    }

    if !by_term.is_empty() {
        println!();
        println!("  by term:");
        for ((bucket, term), count) in &by_term {
            println!("  {bucket:<36} {term:<24} {count:>4}");
        }
    }

    if !findings.is_empty() {
        println!();
        println!("  sites (first 120):");
        for finding in findings.iter().take(120) {
            println!(
                "  {}:{} - {} `{}`: {}",
                finding.rel, finding.line, finding.bucket, finding.term, finding.snippet
            );
        }
        if findings.len() > 120 {
            println!("  ... and {} more", findings.len() - 120);
        }
    }
}

fn finding(
    rel: &str,
    line: usize,
    bucket: &'static str,
    term: &'static str,
    code: &str,
) -> Finding {
    Finding {
        rel: rel.to_string(),
        line,
        bucket,
        term,
        snippet: code.trim().chars().take(140).collect(),
    }
}

fn is_adapter_scan_path(rel: &str) -> bool {
    rel == "crates/tx-shims/src/adapter.rs"
        || rel == "crates/tx-subsystems/src/adapter.rs"
        || (rel.starts_with("crates/tx-subsystems/src/") && rel.ends_with("/adapter.rs"))
}

fn is_wait_raw_scan_path(rel: &str) -> bool {
    (rel.starts_with("crates/tx-subsystems/src/") || rel.starts_with("crates/tx-shims/src/"))
        && rel.ends_with(".rs")
        && !rel.ends_with("/notification.rs")
        && !rel.ends_with("/wait_source.rs")
        && !rel.ends_with("/adapter.rs")
        && rel != "crates/tx-shims/src/adapter.rs"
        && !rel.contains("/substrate/")
        && !rel.contains("/reactor/")
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

    let code = line
        .split_once("//")
        .map_or(line, |(before, _)| before)
        .trim();
    (!code.is_empty()).then_some(code)
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
        let before = start.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
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

#[cfg(test)]
mod tests {
    use super::lint_api_language_text;

    #[test]
    fn api_language_flags_adapter_mechanism_terms() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/adapter.rs",
            r#"
pub use tx_substrate::zone::{IdentitySlot, ZonePolicy};
pub type ReadyMask = Mask;
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "adapter mechanism language" && finding.term == "IdentitySlot"
        }));
        assert!(findings.iter().any(|finding| {
            finding.bucket == "adapter mechanism language" && finding.term == "Mask"
        }));
    }

    #[test]
    fn api_language_flags_wait_raw_terms_outside_allowed_files() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/readiness.rs",
            r#"
fn arm(source_id: WaitSourceId, mask: InterestMask) -> WaitToken {
    source_id.into_raw();
}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "wait readiness raw language" && finding.term == "WaitSourceId"
        }));
        assert!(findings.iter().any(|finding| {
            finding.bucket == "wait readiness raw language" && finding.term == "source_id"
        }));
    }

    #[test]
    fn api_language_skips_wait_raw_terms_in_allowed_wait_files() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/notification.rs",
            "fn notify(token: WaitToken, channel: Channel) { let source_id = 1; }",
        );

        assert!(findings.is_empty());
    }
}
