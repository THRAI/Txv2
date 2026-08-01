//! Hard lint for kernel-side observe producer boundaries.
//!
//! Production producers should emit through typed `HartEmitter` helpers, not by
//! reaching into payload encoders, raw counter records, or ad-hoc event-name
//! hashing. `tx-observe` owns the wire shape and hashing details.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::Result;
use crate::util::{collect_files, relative};

#[derive(Clone, Debug)]
struct Rule {
    name: String,
    replacement: String,
    needles: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ObserveSchema {
    kernel: Option<KernelSchema>,
}

#[derive(Debug, Deserialize)]
struct KernelSchema {
    #[serde(default)]
    producer_boundary_rules: Vec<ProducerBoundaryRuleEntry>,
}

#[derive(Debug, Deserialize)]
struct ProducerBoundaryRuleEntry {
    name: String,
    replacement: String,
    #[serde(default)]
    needles: Vec<String>,
}

const SCAN_ROOTS: &[&str] = &["crates", "boards"];

const ALLOW_PREFIXES: &[&str] = &["crates/tx-observe/", "crates/tx-observe-types/"];

const TEST_PATH_MARKERS: &[&str] = &["/tests/", "/tests.rs", "_tests.rs"];

pub(crate) fn lint_invariants_observe_producer_boundary(root: &Path) -> Result<()> {
    let rules = producer_boundary_rules_from_schema(root)?;
    let mut findings = Vec::new();
    let mut by_rule: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_scanned = 0usize;

    for scan_root in SCAN_ROOTS {
        let dir = root.join(scan_root);
        if !dir.exists() {
            continue;
        }
        for file in collect_files(&dir, &["rs"]).map_err(|err| err.to_string())? {
            let rel = relative(root, &file).replace('\\', "/");
            if is_allowed_path(&rel) {
                continue;
            }
            files_scanned += 1;

            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            for (line_idx, line) in text.lines().enumerate() {
                if line.contains("// observe-producer-boundary: allow ") {
                    continue;
                }
                let Some(code) = code_before_comment(line) else {
                    continue;
                };
                for rule in &rules {
                    if rule.needles.iter().any(|needle| code.contains(needle)) {
                        *by_rule.entry(rule.name.clone()).or_insert(0) += 1;
                        findings.push(format!(
                            "{rel}:{} - {}: {}",
                            line_idx + 1,
                            rule.name,
                            line.trim().chars().take(140).collect::<String>()
                        ));
                        break;
                    }
                }
            }
        }
    }

    println!("Invariants Lint - observe-producer-boundary");
    println!("===========================================");
    println!("files scanned:                         {files_scanned:>4}");
    println!(
        "observe producer boundary findings:    {:>4}",
        findings.len()
    );

    if !by_rule.is_empty() {
        println!();
        println!("  by rule:");
        for (rule, count) in &by_rule {
            println!("  {rule:<36} {count:>4}");
        }
    }
    if !findings.is_empty() {
        println!();
        println!("  sites (first 80):");
        for finding in findings.iter().take(80) {
            println!("  {finding}");
        }
        if findings.len() > 80 {
            println!("  ... and {} more", findings.len() - 80);
        }
        println!();
        println!("  replacements:");
        for rule in &rules {
            println!("  - {}: {}", rule.name, rule.replacement);
        }
    }

    if findings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "observe-producer-boundary lint found {} issue(s)",
            findings.len()
        ))
    }
}

fn producer_boundary_rules_from_schema(root: &Path) -> Result<Vec<Rule>> {
    let schema_path = root.join("schema/txobserve.toml");
    let schema_text = fs::read_to_string(&schema_path)
        .map_err(|err| format!("failed to read {}: {err}", schema_path.display()))?;
    let schema: ObserveSchema = toml::from_str(&schema_text).map_err(|err| {
        format!(
            "schema TOML parse failed in {}: {err}",
            schema_path.display()
        )
    })?;
    let Some(kernel) = schema.kernel else {
        return Err("observe schema has no [kernel] section for producer boundary rules".into());
    };
    let rules: Vec<_> = kernel
        .producer_boundary_rules
        .into_iter()
        .map(|entry| Rule {
            name: entry.name,
            replacement: entry.replacement,
            needles: entry.needles,
        })
        .collect();
    if rules.iter().all(|rule| rule.needles.is_empty()) {
        return Err("observe schema has no producer boundary rule needles".into());
    }
    Ok(rules)
}

fn is_allowed_path(rel: &str) -> bool {
    ALLOW_PREFIXES.iter().any(|prefix| rel.starts_with(prefix))
        || TEST_PATH_MARKERS.iter().any(|marker| rel.contains(marker))
}

fn code_before_comment(line: &str) -> Option<&str> {
    let code = line
        .split_once("//")
        .map_or(line, |(before, _)| before)
        .trim();
    if code.is_empty() { None } else { Some(code) }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{unique}", std::process::id()))
    }

    fn write_default_schema(root: &std::path::Path) {
        let schema_dir = root.join("schema");
        fs::create_dir_all(&schema_dir).expect("create temp schema dir");
        fs::write(
            schema_dir.join("txobserve.toml"),
            r#"
[kernel]

[[kernel.producer_boundary_rules]]
name = "raw observe payload encoder"
replacement = "Add or use a typed HartEmitter helper in tx-observe."
needles = ["tx_observe::encode::", "use tx_observe::encode"]

[[kernel.producer_boundary_rules]]
name = "raw debug counter emit"
replacement = "Use HartEmitter::debug_counter(name, value)."
needles = ["observer.counter("]

[[kernel.producer_boundary_rules]]
name = "ad-hoc event-name hash"
replacement = "Use EventNameId::from_name(name) or a typed HartEmitter helper."
needles = [
  "EventNameId::from_raw(tx_observe::fnv1a32(",
  "tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(",
]
"#,
        )
        .expect("write temp observe schema");
    }

    #[test]
    fn observe_producer_boundary_fails_on_raw_encoder_counter_and_hash() {
        let root = temp_root("tx-observe-producer-boundary-lint");
        let crate_dir = root.join("crates/bad/src");
        fs::create_dir_all(&crate_dir).expect("create temp crate dir");
        write_default_schema(&root);
        fs::write(
            crate_dir.join("lib.rs"),
            r#"
use tx_observe::encode;

fn bad(observer: tx_observe::HartEmitter) {
    let _ = tx_observe::encode::encode_syscall_enter;
    let _name = tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(b"debug.bad.name"));
    observer.counter(tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(b"debug.bad")), 1);
}
"#,
        )
        .expect("write temp bad source");

        let result = super::lint_invariants_observe_producer_boundary(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("raw observe producer APIs should fail the hard lint");
        assert!(err.contains("observe-producer-boundary lint found"));
    }

    #[test]
    fn observe_producer_boundary_allows_typed_helpers_and_domain_counters() {
        let root = temp_root("tx-observe-producer-boundary-clean");
        let crate_dir = root.join("crates/good/src");
        fs::create_dir_all(&crate_dir).expect("create temp crate dir");
        write_default_schema(&root);
        fs::write(
            crate_dir.join("lib.rs"),
            r#"
fn good(observer: tx_observe::HartEmitter, eventfd: EventFd) {
    observer.debug_counter(b"debug.good", 1);
    let _ = eventfd.counter();
}
"#,
        )
        .expect("write temp clean source");

        let result = super::lint_invariants_observe_producer_boundary(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        assert!(result.is_ok(), "typed observe helper use should pass");
    }

    #[test]
    fn observe_producer_boundary_uses_schema_rules() {
        let root = temp_root("tx-observe-producer-boundary-schema-rules");
        let crate_dir = root.join("crates/bad/src");
        let schema_dir = root.join("schema");
        fs::create_dir_all(&crate_dir).expect("create temp crate dir");
        fs::create_dir_all(&schema_dir).expect("create temp schema dir");
        fs::write(
            schema_dir.join("txobserve.toml"),
            r#"
[kernel]

[[kernel.producer_boundary_rules]]
name = "schema-owned observe marker"
replacement = "Use the schema-owned helper."
needles = ["schema_forbidden_observe_marker("]
"#,
        )
        .expect("write temp schema");
        fs::write(
            crate_dir.join("lib.rs"),
            r#"
fn bad() {
    schema_forbidden_observe_marker();
}
"#,
        )
        .expect("write temp bad source");

        let result = super::lint_invariants_observe_producer_boundary(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("schema-owned observe rule should fail the hard lint");
        assert!(err.contains("observe-producer-boundary lint found"));
    }
}
