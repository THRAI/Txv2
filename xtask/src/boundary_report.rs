//! `cargo xtask boundary-report` — count raw substrate/reactor usage outside
//! sanctioned adapter modules.
//!
//! Produces the Architecture Boundary Report so convergence work has a
//! monotonic ratchet number to drive down. This is a counter, not a gate:
//! it never fails. The `lint boundary` enforcement comes after adapter
//! modules exist.
//!
//! Adapter modules are recognised by the `#[platform_adapter(platform =
//! "...", domain = "...", reason = "...")]` attribute. Raw substrate calls
//! in a file that declares such an attribute (for the matching platform)
//! count as `inside_adapter`; everywhere else counts as `outside_adapter`,
//! which is the number that should burn down to zero.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

struct Platform {
    name: &'static str,
    home_crate: &'static str,
    import_root: &'static str,
}

const PLATFORMS: &[Platform] = &[
    Platform {
        name: "substrate",
        home_crate: "crates/tx-substrate/",
        import_root: "tx_substrate",
    },
    Platform {
        name: "reactor",
        home_crate: "crates/tx-reactor/",
        import_root: "tx_reactor",
    },
];

#[derive(Default, Debug)]
struct PlatformStats {
    raw_lines_outside_adapter: usize,
    files_outside_adapter: usize,
    raw_lines_inside_adapter: usize,
    files_inside_adapter: usize,
    sub_api_lines: BTreeMap<String, usize>,
    per_file_lines_outside: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
struct AdapterDecl {
    file: String,
    platform: String,
    domain: String,
    reason: String,
}

pub(crate) fn boundary_report(root: &Path, args: Vec<String>) -> Result<()> {
    let json = args.iter().any(|a| a == "--json");
    let top_n: usize = args
        .iter()
        .position(|a| a == "--top")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let files = collect_files(root, &["rs"]).map_err(|err| err.to_string())?;
    let mut stats_substrate = PlatformStats::default();
    let mut stats_reactor = PlatformStats::default();
    let mut adapters: Vec<AdapterDecl> = Vec::new();

    for file in files {
        let rel = relative(root, &file).replace('\\', "/");
        if !rel.starts_with("crates/") {
            continue;
        }
        if is_meta_crate(&rel) {
            // The macro crate itself defines `#[platform_adapter]`; its
            // own source and tests mention the attribute literally for
            // documentation and end-to-end expansion checks, but those
            // are not workspace adapter declarations and must not be
            // counted as such.
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
        let scan = scan_file(&rel, &text);
        adapters.extend(scan.adapters.iter().cloned());
        accumulate(&PLATFORMS[0], &rel, &scan, &mut stats_substrate);
        accumulate(&PLATFORMS[1], &rel, &scan, &mut stats_reactor);
    }

    if json {
        emit_json(&stats_substrate, &stats_reactor, &adapters);
    } else {
        emit_human(&stats_substrate, &stats_reactor, &adapters, top_n);
    }
    Ok(())
}

fn is_meta_crate(rel: &str) -> bool {
    rel.starts_with("crates/tx-platform-adapter/")
}

fn accumulate(p: &Platform, rel: &str, scan: &FileScan, stats: &mut PlatformStats) {
    if rel.starts_with(p.home_crate) {
        return;
    }
    let lines = scan.line_counts.get(p.import_root).copied().unwrap_or(0);
    if lines == 0 {
        return;
    }
    let sanctioned = scan.adapters.iter().any(|a| a.platform == p.name);
    if sanctioned {
        stats.raw_lines_inside_adapter += lines;
        stats.files_inside_adapter += 1;
    } else {
        stats.raw_lines_outside_adapter += lines;
        stats.files_outside_adapter += 1;
        stats
            .per_file_lines_outside
            .insert(rel.to_string(), lines);
    }
    let needle = format!("{}::", p.import_root);
    for (sub_root, count) in &scan.sub_api_counts {
        if let Some(rest) = sub_root.strip_prefix(&needle) {
            let leaf = rest.split("::").next().unwrap_or(rest);
            *stats.sub_api_lines.entry(leaf.to_string()).or_default() += count;
        }
    }
}

#[derive(Default, Debug)]
struct FileScan {
    /// `tx_substrate` / `tx_reactor` -> number of lines that mention it.
    line_counts: BTreeMap<String, usize>,
    /// fully-prefixed sub-API segment (e.g. `tx_substrate::step_v3`) ->
    /// total line-level occurrences across the file.
    sub_api_counts: BTreeMap<String, usize>,
    adapters: Vec<AdapterDecl>,
}

fn scan_file(rel: &str, text: &str) -> FileScan {
    let mut scan = FileScan::default();
    for line in text.lines() {
        for root in ["tx_substrate", "tx_reactor"] {
            let needle = format!("{root}::");
            if line.contains(&needle) {
                *scan.line_counts.entry(root.to_string()).or_default() += 1;
                let mut rest = line;
                while let Some(pos) = rest.find(&needle) {
                    let after = &rest[pos + needle.len()..];
                    let end = after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(after.len());
                    if end == 0 {
                        rest = after.get(1..).unwrap_or("");
                        continue;
                    }
                    let sub = &after[..end];
                    *scan
                        .sub_api_counts
                        .entry(format!("{root}::{sub}"))
                        .or_default() += 1;
                    rest = &after[end..];
                }
            }
        }
    }
    scan.adapters = extract_adapters(rel, text);
    scan
}

fn extract_adapters(rel: &str, text: &str) -> Vec<AdapterDecl> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("#[platform_adapter(") {
        let after = &rest[pos + "#[platform_adapter(".len()..];
        let Some(end) = after.find(")]") else {
            break;
        };
        let body = &after[..end];
        out.push(AdapterDecl {
            file: rel.to_string(),
            platform: extract_kv(body, "platform"),
            domain: extract_kv(body, "domain"),
            reason: extract_kv(body, "reason"),
        });
        rest = &after[end + 2..];
    }
    out
}

fn extract_kv(body: &str, key: &str) -> String {
    let Some(pos) = body.find(key) else {
        return String::new();
    };
    let after = &body[pos + key.len()..];
    let Some(eq) = after.find('=') else {
        return String::new();
    };
    let after = after[eq + 1..].trim_start();
    let Some(rest) = after.strip_prefix('"') else {
        return String::new();
    };
    rest.find('"').map(|end| rest[..end].to_string()).unwrap_or_default()
}

fn emit_human(
    s: &PlatformStats,
    r: &PlatformStats,
    adapters: &[AdapterDecl],
    top_n: usize,
) {
    println!("Architecture Boundary Report");
    println!("============================");
    println!();
    println!(
        "Raw substrate calls outside adapters: {:>5} lines / {:>3} files",
        s.raw_lines_outside_adapter, s.files_outside_adapter
    );
    println!(
        "Raw substrate calls inside  adapters: {:>5} lines / {:>3} files",
        s.raw_lines_inside_adapter, s.files_inside_adapter
    );
    println!(
        "Raw reactor   calls outside adapters: {:>5} lines / {:>3} files",
        r.raw_lines_outside_adapter, r.files_outside_adapter
    );
    println!(
        "Raw reactor   calls inside  adapters: {:>5} lines / {:>3} files",
        r.raw_lines_inside_adapter, r.files_inside_adapter
    );
    println!("Platform adapters declared:           {:>5}", adapters.len());
    println!();
    print_sub_api(
        "substrate sub-API fan-in (lines, outside-adapter occurrences):",
        &s.sub_api_lines,
    );
    println!();
    print_sub_api(
        "reactor sub-API fan-in (lines, outside-adapter occurrences):",
        &r.sub_api_lines,
    );
    println!();
    print_top_files(
        &format!("top {top_n} substrate offenders (outside adapters):"),
        &s.per_file_lines_outside,
        top_n,
    );
    println!();
    print_top_files(
        &format!("top {top_n} reactor offenders (outside adapters):"),
        &r.per_file_lines_outside,
        top_n,
    );
    println!();
    if adapters.is_empty() {
        println!("adapters: none (#[platform_adapter] not yet adopted)");
    } else {
        println!("adapters:");
        for a in adapters {
            println!("  {:>9} :: {:<24} {}", a.platform, a.domain, a.file);
            if !a.reason.is_empty() {
                println!("    reason: {}", a.reason);
            }
        }
    }
}

fn print_sub_api(label: &str, m: &BTreeMap<String, usize>) {
    println!("{label}");
    if m.is_empty() {
        println!("  (none)");
        return;
    }
    let mut rows: Vec<(&String, &usize)> = m.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (name, count) in rows {
        println!("  {count:>6}  {name}");
    }
}

fn print_top_files(label: &str, m: &BTreeMap<String, usize>, n: usize) {
    println!("{label}");
    if m.is_empty() {
        println!("  (none)");
        return;
    }
    let mut rows: Vec<(&String, &usize)> = m.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (name, count) in rows.into_iter().take(n) {
        println!("  {count:>5}  {name}");
    }
}

fn emit_json(
    s: &PlatformStats,
    r: &PlatformStats,
    adapters: &[AdapterDecl],
) {
    let value = serde_json::json!({
        "substrate": platform_json(s),
        "reactor": platform_json(r),
        "adapters": adapters
            .iter()
            .map(|a| serde_json::json!({
                "file": a.file,
                "platform": a.platform,
                "domain": a.domain,
                "reason": a.reason,
            }))
            .collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
}

fn platform_json(s: &PlatformStats) -> serde_json::Value {
    serde_json::json!({
        "raw_lines_outside_adapter": s.raw_lines_outside_adapter,
        "files_outside_adapter": s.files_outside_adapter,
        "raw_lines_inside_adapter": s.raw_lines_inside_adapter,
        "files_inside_adapter": s.files_inside_adapter,
        "sub_api_lines": s.sub_api_lines,
        "per_file_lines_outside": s.per_file_lines_outside,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_counts_substrate_lines_and_sub_apis() {
        let scan = scan_file(
            "crates/tx-subsystems/src/vfs/execution.rs",
            "use tx_substrate::step_v3::StepOutcome;\n\
             use tx_substrate::epoch::guard;\n\
             fn f() { tx_substrate::step_v3::run(); }\n",
        );
        assert_eq!(scan.line_counts["tx_substrate"], 3);
        assert_eq!(scan.sub_api_counts["tx_substrate::step_v3"], 2);
        assert_eq!(scan.sub_api_counts["tx_substrate::epoch"], 1);
    }

    #[test]
    fn scan_counts_multiple_sub_apis_on_one_line() {
        // `use tx_substrate::{a::A, b::B};` is two top-level fan-in slots
        // because each appears as `tx_substrate::` on the same line; the
        // line is still one line for `line_counts`.
        let scan = scan_file(
            "crates/foo.rs",
            "let _ = (tx_substrate::epoch::guard(), tx_substrate::zone::id());\n",
        );
        assert_eq!(scan.line_counts["tx_substrate"], 1);
        assert_eq!(scan.sub_api_counts["tx_substrate::epoch"], 1);
        assert_eq!(scan.sub_api_counts["tx_substrate::zone"], 1);
    }

    #[test]
    fn scan_counts_reactor_independently() {
        let scan = scan_file(
            "crates/tx-subsystems/src/pipe.rs",
            "use tx_reactor::wait::WaitSource;\nuse tx_substrate::epoch::guard;\n",
        );
        assert_eq!(scan.line_counts["tx_reactor"], 1);
        assert_eq!(scan.line_counts["tx_substrate"], 1);
        assert_eq!(scan.sub_api_counts["tx_reactor::wait"], 1);
    }

    #[test]
    fn extract_adapters_parses_full_attribute() {
        let decls = extract_adapters(
            "crates/tx-subsystems/src/vfs/binding_adapter.rs",
            "#[platform_adapter(\n\
                 platform = \"substrate\",\n\
                 domain = \"vfs\",\n\
                 reason = \"translate index publication into VFS binding\"\n\
             )]\n\
             pub mod binding_adapter {}",
        );
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].platform, "substrate");
        assert_eq!(decls[0].domain, "vfs");
        assert!(decls[0].reason.contains("VFS binding"));
    }

    #[test]
    fn adapter_file_lines_count_as_inside() {
        let scan = scan_file(
            "crates/tx-subsystems/src/vfs/binding_adapter.rs",
            "#[platform_adapter(platform = \"substrate\", domain = \"vfs\", reason = \"r\")]\n\
             pub mod binding_adapter {\n\
                 use tx_substrate::index::Index;\n\
                 fn f() { tx_substrate::index::lookup(); }\n\
             }",
        );
        let mut stats = PlatformStats::default();
        accumulate(
            &PLATFORMS[0],
            "crates/tx-subsystems/src/vfs/binding_adapter.rs",
            &scan,
            &mut stats,
        );
        assert_eq!(stats.raw_lines_outside_adapter, 0);
        assert_eq!(stats.files_outside_adapter, 0);
        assert_eq!(stats.raw_lines_inside_adapter, 2);
        assert_eq!(stats.files_inside_adapter, 1);
    }

    #[test]
    fn non_adapter_file_lines_count_as_outside() {
        let scan = scan_file(
            "crates/tx-subsystems/src/vfs/execution.rs",
            "use tx_substrate::step_v3::StepOutcome;\n",
        );
        let mut stats = PlatformStats::default();
        accumulate(
            &PLATFORMS[0],
            "crates/tx-subsystems/src/vfs/execution.rs",
            &scan,
            &mut stats,
        );
        assert_eq!(stats.raw_lines_outside_adapter, 1);
        assert_eq!(stats.files_outside_adapter, 1);
        assert_eq!(stats.raw_lines_inside_adapter, 0);
        assert_eq!(stats.sub_api_lines["step_v3"], 1);
    }

    #[test]
    fn home_crate_files_are_skipped() {
        let scan = scan_file(
            "crates/tx-substrate/src/lib.rs",
            "use tx_substrate::epoch::guard;\n",
        );
        let mut stats = PlatformStats::default();
        accumulate(
            &PLATFORMS[0],
            "crates/tx-substrate/src/lib.rs",
            &scan,
            &mut stats,
        );
        assert_eq!(stats.raw_lines_outside_adapter, 0);
        assert_eq!(stats.raw_lines_inside_adapter, 0);
        assert!(stats.sub_api_lines.is_empty());
    }

    #[test]
    fn adapter_for_other_platform_does_not_sanction() {
        // A file that declares a reactor adapter must still have its raw
        // substrate calls counted as outside-adapter for the substrate
        // platform.
        let scan = scan_file(
            "crates/tx-subsystems/src/mixed.rs",
            "#[platform_adapter(platform = \"reactor\", domain = \"x\", reason = \"r\")]\n\
             pub mod m { use tx_substrate::epoch::guard; }",
        );
        let mut stats = PlatformStats::default();
        accumulate(
            &PLATFORMS[0],
            "crates/tx-subsystems/src/mixed.rs",
            &scan,
            &mut stats,
        );
        assert_eq!(stats.raw_lines_outside_adapter, 1);
        assert_eq!(stats.raw_lines_inside_adapter, 0);
    }
}
