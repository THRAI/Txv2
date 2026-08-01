//! Lint rule for legacy EBR / Zone interface vocabulary.
//!
//! `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md` maps the imported
//! OSTD-local EBR/Zone API onto txKernel's object-model vocabulary. Production
//! code should use `epoch::guard`, `Guard`, `Weak<T>`, `ZoneReservation<T>`,
//! `zone::sign`, and hidden policy selection. The legacy spellings below are a
//! migration smell in runtime code.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

const MAX_LEGACY_ZONE_INTERFACE_SITES: usize = 0;

const TARGET_DIRS: &[&str] = &[
    "boards",
    "crates/tx-kernel/src",
    "crates/tx-reactor/src",
    "crates/tx-scripts/src",
    "crates/tx-shims/src",
    "crates/tx-substrate/src",
    "crates/tx-subsystems/src",
];

pub(crate) fn lint_invariants_zone_interface(root: &Path) -> Result<()> {
    let mut sites = Vec::new();
    let mut by_rule: BTreeMap<&'static str, usize> = BTreeMap::new();

    for dir in TARGET_DIRS {
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
            for (line_idx, line) in text.lines().enumerate() {
                let Some(code) = code_before_comment(line) else {
                    continue;
                };
                let Some((rule, detail)) = legacy_zone_interface_violation(&rel, code) else {
                    continue;
                };
                *by_rule.entry(rule).or_insert(0) += 1;
                sites.push(format!(
                    "{rel}:{} - {detail}: {}",
                    line_idx + 1,
                    code.trim().chars().take(120).collect::<String>()
                ));
            }
        }
    }

    let count = sites.len();
    let over = count > MAX_LEGACY_ZONE_INTERFACE_SITES;

    println!("Invariants Lint - zone-interface");
    println!("=================================");
    println!(
        "legacy EBR/Zone interface sites: {:>4}  (ceiling {})  {}",
        count,
        MAX_LEGACY_ZONE_INTERFACE_SITES,
        if over { "OVER" } else { "ok" }
    );

    if !by_rule.is_empty() {
        println!();
        println!("  by rule:");
        for (rule, n) in &by_rule {
            println!("  {rule:<24} {n:>4}");
        }
    }

    if !sites.is_empty() {
        println!();
        println!("  sites (first 80):");
        for site in sites.iter().take(80) {
            println!("  {site}");
        }
        if sites.len() > 80 {
            println!("  ... and {} more", sites.len() - 80);
        }
    }

    if over {
        return Err(format!(
            "zone-interface ratchet regression - {count} legacy EBR/Zone interface sites > ceiling {MAX_LEGACY_ZONE_INTERFACE_SITES}. Use epoch::guard/Guard, Weak<T>, ZoneReservation<T>, zone::sign, and role-shaped Cap/PayloadCap/IdentRef APIs; keep policy selection hidden in substrate or entity-zone declarations."
        ));
    }

    Ok(())
}

fn legacy_zone_interface_violation(rel: &str, code: &str) -> Option<(&'static str, String)> {
    if code.contains("epoch::pin") {
        return Some((
            "epoch-pin",
            "`epoch::pin` is legacy vocabulary; use `epoch::guard`".into(),
        ));
    }
    if contains_ident(code, "WeakCap") {
        return Some((
            "weak-cap",
            "`WeakCap` is legacy retaining-weak vocabulary; use non-retaining `Weak<T>`".into(),
        ));
    }
    if contains_ident(code, "weak_count") {
        return Some((
            "weak-count",
            "`weak_count` must not participate in semantic zone reclamation".into(),
        ));
    }
    if contains_ident(code, "ReservedSlot") {
        return Some((
            "reserved-slot",
            "`ReservedSlot` is legacy vocabulary; use `ZoneReservation<T>`".into(),
        ));
    }
    if code.contains("ReservedSlot::init") {
        return Some((
            "reserved-slot-init",
            "`ReservedSlot::init` is legacy publication; use `zone::sign`".into(),
        ));
    }
    if code.contains("Cap::get") {
        return Some((
            "cap-get",
            "`Cap::get` is legacy vocabulary; use deref or `Cap::ident_ref(&Guard)`".into(),
        ));
    }
    if contains_ident(code, "RcPolicy") || contains_ident(code, "EbrPolicy") {
        return Some((
            "old-policy-name",
            "`RcPolicy`/`EbrPolicy` must not appear in production code".into(),
        ));
    }
    if contains_ident(code, "EpochGuard") && !rel.starts_with("crates/tx-substrate/src/epoch/") {
        return Some((
            "epoch-guard-name",
            "`EpochGuard` is legacy public vocabulary; use `Guard`".into(),
        ));
    }
    if raw_zone_policy_surface(code) && !zone_policy_allowed(rel) {
        return Some((
            "raw-zone-policy",
            "raw `Zone<T, Policy>` surface outside substrate/entity-zone boundary".into(),
        ));
    }
    None
}

fn raw_zone_policy_surface(code: &str) -> bool {
    let compact: String = code.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.contains("Zone<") && compact.contains(",") && compact.contains("Policy")
}

fn zone_policy_allowed(rel: &str) -> bool {
    rel.starts_with("crates/tx-substrate/src/zone/")
        || rel == "crates/tx-subsystems/src/zones.rs"
        || rel.ends_with("/zones.rs")
}

fn should_skip_file(rel: &str) -> bool {
    rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
        || rel.ends_with("_tests.rs")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_legacy_epoch_pin() {
        let finding = legacy_zone_interface_violation(
            "crates/tx-subsystems/src/vfs/mod.rs",
            "let g = epoch::pin();",
        );
        assert!(matches!(finding, Some(("epoch-pin", _))));
    }

    #[test]
    fn rejects_upper_raw_zone_policy() {
        let finding = legacy_zone_interface_violation(
            "crates/tx-subsystems/src/process/execution.rs",
            "type P = Zone<ProcessIdentity, EbrPolicy>;",
        );
        assert!(matches!(
            finding,
            Some(("old-policy-name", _)) | Some(("raw-zone-policy", _))
        ));
    }

    #[test]
    fn allows_substrate_hidden_zone_policy() {
        let finding = legacy_zone_interface_violation(
            "crates/tx-substrate/src/zone/policy.rs",
            "pub(crate) struct Zone<T, P: SlotPolicy> { _marker: PhantomData<(T, P)> }",
        );
        assert!(finding.is_none());
    }

    #[test]
    fn allows_non_zone_tombstone_vocabulary() {
        let finding = legacy_zone_interface_violation(
            "boards/tx-hal-riscv64-qemu-virt/src/pmap/pt_node.rs",
            "CommittedPtNodeEntry::Tombstone => {}",
        );
        assert!(finding.is_none());
    }
}
