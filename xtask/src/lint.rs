use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::target::{installed_targets, target_triple, TxTarget};
use crate::util::{collect_files, relative, shell_join};
use crate::Result;

const MAX_AUTHORED_RUST_FILE_LINES: usize = 1_500;

pub(crate) fn lint(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("lint command needs `arch`, `docs`, or `unused`".into());
    };
    match kind.as_str() {
        "arch" => lint_arch(root),
        "docs" => lint_docs(root),
        "unused" => lint_unused(root),
        other => Err(format!(
            "unknown lint kind '{other}', expected arch, docs, or unused"
        )),
    }
}

pub(crate) fn lint_arch(root: &Path) -> Result<()> {
    let mut findings = Vec::new();
    let files = collect_files(root, &["rs", "toml"]).map_err(|err| err.to_string())?;

    for file in files {
        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
        let relative = relative(root, &file);
        let normalized = relative.replace('\\', "/");

        if normalized.starts_with("target/") || normalized.starts_with("external/") {
            continue;
        }
        if let Some(finding) = lint_file_size(&normalized, &relative, &text) {
            findings.push(finding);
        }
        if normalized.starts_with("xtask/") {
            continue;
        }

        findings.extend(lint_arch_text(&normalized, &relative, &text));
    }

    if findings.is_empty() {
        println!("arch lint: ok");
        Ok(())
    } else {
        for finding in &findings {
            eprintln!("{finding}");
        }
        Err(format!("arch lint found {} issue(s)", findings.len()))
    }
}

pub(crate) fn lint_docs(root: &Path) -> Result<()> {
    let files = collect_files(root, &["md"]).map_err(|err| err.to_string())?;
    let mut errors = Vec::new();
    let mut stale_warnings = 0usize;
    let mut txdoc_tags = BTreeMap::<String, String>::new();

    for file in files {
        let normalized = relative(root, &file).replace('\\', "/");
        if normalized.contains("/archived/") || normalized.starts_with("external/") {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
        errors.extend(check_markdown_links(root, &file, &text));
        stale_warnings += count_stale_doc_mentions(&text);

        let tags = extract_txdoc_tags(&text);
        let active_design_doc = normalized.starts_with("docs/design/");
        if active_design_doc {
            if tags.is_empty() {
                errors.push(format!("{normalized}: missing file-level txdoc tag"));
            } else if tags.len() < 2 {
                errors.push(format!(
                    "{normalized}: missing fine-grained txdoc section anchors"
                ));
            }
        }
        for (line_no, tag) in tags {
            if !valid_txdoc_tag(&tag) {
                errors.push(format!("{normalized}:{line_no}: invalid txdoc tag `{tag}`"));
            }
            let here = format!("{normalized}:{line_no}");
            if let Some(previous) = txdoc_tags.insert(tag.clone(), here.clone()) {
                errors.push(format!(
                    "{here}: duplicate txdoc tag `{tag}` previously declared at {previous}"
                ));
            }
        }
    }

    if stale_warnings > 0 {
        println!("docs lint: {stale_warnings} stale-vocabulary mention(s) found in active docs; treated as warnings because current docs discuss retired terms");
    }

    // Harvest TXV3 tag references from Rust sources under crates/, boards/,
    // xtask/ and verify each resolves to a tag declared in some Txv3 (or
    // design) doc. Tags from docs/Txv3/ are already in `txdoc_tags` because
    // the markdown pass above scans all non-archived `.md` files.
    let known: BTreeSet<String> = txdoc_tags.keys().cloned().collect();
    let rust_files = collect_files(root, &["rs"]).map_err(|err| err.to_string())?;
    let mut rust_payload: Vec<(String, String)> = Vec::new();
    for file in rust_files {
        let normalized = relative(root, &file).replace('\\', "/");
        if normalized.starts_with("target/") || normalized.starts_with("external/") {
            continue;
        }
        if !(normalized.starts_with("crates/")
            || normalized.starts_with("boards/")
            || normalized.starts_with("xtask/"))
        {
            continue;
        }
        // The linter implementation itself necessarily contains test
        // fixtures and self-describing prose that mention `txdoc:TXV3-*`
        // tags; do not lint the linter against itself.
        if normalized == "xtask/src/lint.rs" {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
        rust_payload.push((normalized, text));
    }
    let borrowed: Vec<(&str, &str)> = rust_payload
        .iter()
        .map(|(d, t)| (d.as_str(), t.as_str()))
        .collect();
    errors.extend(lint_txv3_code_references(&known, &borrowed));

    if errors.is_empty() {
        println!("docs lint: ok");
        Ok(())
    } else {
        for error in &errors {
            eprintln!("{error}");
        }
        Err(format!("docs lint found {} broken link(s)", errors.len()))
    }
}

pub(crate) fn lint_unused(root: &Path) -> Result<()> {
    let installed = installed_targets().unwrap_or_default();
    let steps = unused_check_steps(&installed);

    for step in steps {
        if let Some(reason) = step.skip_reason {
            println!("skip: unused lint {}: {reason}", step.name);
            continue;
        }
        run_unused_check(root, &step.args)?;
    }

    println!("unused lint: ok");
    Ok(())
}

fn extract_txdoc_tags(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let Some(start) = line.find("<!-- txdoc:") else {
            continue;
        };
        let rest = &line[start + "<!-- txdoc:".len()..];
        let Some(end) = rest.find("-->") else {
            continue;
        };
        out.push((idx + 1, rest[..end].trim().to_string()));
    }
    out
}

fn valid_txdoc_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.chars().all(|ch| {
            ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.' | ':')
        })
}

fn check_markdown_links(root: &Path, file: &Path, text: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(start) = rest.find("](") {
            rest = &rest[start + 2..];
            let link;
            if rest.starts_with('<') {
                let Some(end) = rest.find(">)") else {
                    break;
                };
                link = &rest[..end + 1];
                rest = &rest[end + 2..];
            } else {
                let Some(end) = rest.find(')') else {
                    break;
                };
                link = &rest[..end];
                rest = &rest[end + 1..];
            };
            let mut link = link;
            if link.starts_with("http://")
                || link.starts_with("https://")
                || link.starts_with('#')
                || link.starts_with("mailto:")
            {
                continue;
            }
            if link.starts_with('<') && link.ends_with('>') {
                link = &link[1..link.len() - 1];
            }
            let path_part = link.split('#').next().unwrap_or(link);
            if path_part.is_empty() {
                continue;
            }
            let candidate = file
                .parent()
                .unwrap_or(root)
                .join(path_part)
                .components()
                .collect::<PathBuf>();
            if !candidate.exists() {
                errors.push(format!(
                    "{}:{}: broken markdown link `{}`",
                    relative(root, file),
                    line_idx + 1,
                    link
                ));
            }
        }
    }
    errors
}

fn count_stale_doc_mentions(text: &str) -> usize {
    [
        "v11",
        "OSTD HAL manager",
        "HalManager",
        "Box<dyn Hal>",
        "dispatcher/",
        "pipeline/",
    ]
    .iter()
    .map(|term| text.matches(term).count())
    .sum()
}

fn zone_policy_allowed(path: &str) -> bool {
    path.starts_with("crates/tx-substrate/")
        || path.contains("/zone/")
        || path.contains("entity_zone")
        || path.contains("entity-zone")
}

fn board_import_allowed(path: &str, arch: &str) -> bool {
    path.starts_with(&format!("boards/tx-kernel-{arch}-qemu-virt/"))
        || path.starts_with(&format!("boards/tx-hal-{arch}-qemu-virt/"))
        || (arch == "riscv64"
            && (path.starts_with("boards/tx-kernel-riscv64-m1dock-mock/")
                || path.starts_with("boards/tx-hal-riscv64-m1dock-mock/")))
}

fn boot_static_capture_allowed(path: &str) -> bool {
    path == "boards/tx-hal-riscv64-qemu-virt/src/boot_static.rs"
}

fn rv64_qemu_boot_static_path(path: &str) -> bool {
    path.starts_with("boards/tx-hal-riscv64-qemu-virt/src/")
}

fn unused_allowance(line: &str) -> bool {
    (line.contains("#[allow(") || line.contains("#![allow("))
        && (line.contains("dead_code")
            || line.contains("unused")
            || line.contains("unused_imports")
            || line.contains("unused_variables"))
}

fn lint_arch_text(path: &str, display: &str, text: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if unused_allowance(line) {
            findings.push(format!(
                "{display}:{line_no}: unused/dead-code allowances hide stale boot and API surfaces; remove the item or gate it behind cfg(test)"
            ));
        }
        if path.starts_with("crates/tx-kernel/") && line.contains("#[cfg(target_arch") {
            findings.push(format!(
                "{display}:{line_no}: tx-kernel must not cfg on target_arch"
            ));
        }
        if path.starts_with("crates/tx-kernel/")
            && (line.contains("BootArg")
                || line.contains("firmware_arg")
                || line.contains("rust_entry")
                || line.contains("fn _start")
                || line.contains("extern \"C\" fn _start"))
        {
            findings.push(format!(
                "{display}:{line_no}: tx-kernel must consume BootHandoff, not raw firmware boot values"
            ));
        }
        if path.starts_with("boards/tx-kernel-")
            && (line.contains("fn _start")
                || line.contains("extern \"C\" fn _start")
                || line.contains(".globl _start"))
        {
            findings.push(format!(
                "{display}:{line_no}: board kernel binaries must export rust_entry only; platform crates own _start"
            ));
        }
        if line.contains("HalManager")
            || line.contains("Box<dyn Hal>")
            || line.contains("dyn Hal")
            || line.contains("__ostd_main")
        {
            findings.push(format!(
                "{display}:{line_no}: resurrected runtime HAL vocabulary"
            ));
        }
        if line.contains("Zone<") && line.contains("Policy") && !zone_policy_allowed(path) {
            findings.push(format!(
                "{display}:{line_no}: raw Zone<T, Policy> outside substrate/entity-zone boundary"
            ));
        }
        if line.contains("tx_hal_riscv64_qemu_virt") && !board_import_allowed(path, "riscv64") {
            findings.push(format!(
                "{display}:{line_no}: concrete RV64 platform imported outside board boundary"
            ));
        }
        if line.contains("tx_hal_riscv64_m1dock_mock") && !board_import_allowed(path, "riscv64") {
            findings.push(format!(
                "{display}:{line_no}: concrete RV64 M1 Dock mock platform imported outside board boundary"
            ));
        }
        if line.contains("tx_hal_loongarch64_qemu_virt")
            && !board_import_allowed(path, "loongarch64")
        {
            findings.push(format!(
                "{display}:{line_no}: concrete LA64 platform imported outside board boundary"
            ));
        }
        if rv64_qemu_boot_static_path(path)
            && !boot_static_capture_allowed(path)
            && (line.contains("addr_of!")
                || line.contains("addr_of_mut!")
                || line.contains(".get() as usize"))
        {
            findings.push(format!(
                "{display}:{line_no}: RV64 QEMU boot-static capture must go through boot_static.rs"
            ));
        }
        // TTY-CTL-1 (OBJECT_PATTERN_FIXES_v1.md OPA-3): TTY identity
        // structure must not store leader-process caps as a substitute
        // for session/pgrp truth. The controlling-terminal binding
        // names a Session and a foreground ProcessGroup, both held as
        // Weak refs (no retention).
        if path == "crates/tx-subsystems/src/tty/structure/identity.rs"
            && line.contains("Cap<ProcessIdentity>")
        {
            findings.push(format!(
                "{display}:{line_no}: TTY-CTL-1 violation — TTY identity structure must not store Cap<ProcessIdentity>; use Weak<Session>/Weak<ProcessGroup> for the controlling-terminal binding"
            ));
        }
        // TTY-CTL-1a (OBJECT_PATTERN_FIXES_v1.md OPA-3): the
        // foreground-pgrp slot is TTY-owned (TtyIdentity.session_pgrp).
        // Process-side structs must not declare their own
        // foreground_pgrp field; readers use Session::foreground_pgrp_cap()
        // for the two-hop dereference.
        if path == "crates/tx-subsystems/src/process/structure.rs"
            && line.contains("foreground_pgrp:")
            && !line.trim_start().starts_with("///")
            && !line.trim_start().starts_with("//!")
            && !line.trim_start().starts_with("//")
        {
            findings.push(format!(
                "{display}:{line_no}: TTY-CTL-1a violation — process-side structs must not declare a `foreground_pgrp` field (authoritative slot lives on TtyIdentity.session_pgrp; use Session::foreground_pgrp_cap() for the two-hop weak dereference)"
            ));
        }
    }
    // A-3: `.await` inside a `fn step(` body. Per
    // `docs/Txv3/03_STEP_MODEL_v2.md` §10 (STEP-2): step functions are
    // synchronous bounded transactions; suspension is expressed via
    // `Yield { shape, .. }`, not via `.await`. The driver composes.
    findings.extend(lint_step_no_await(display, text));
    findings
}

/// A-3 detector: walk the file, track brace depth, and emit a finding
/// for any `.await` whose enclosing `{ ... }` block was opened by a
/// `fn step(` signature.
///
/// Heuristics: line-level scan with comment stripping (`//` only —
/// block comments not handled), exact substring match on `fn step(`
/// (so `fn step_foo(` does not trigger). The signature, the `{`, and
/// the `.await` may all be on the same line; the scan resolves all
/// three in one character-by-character pass per line.
/// Scan Rust source `text` for `txdoc:TXV3-*` references inside comments
/// and return `(line_no, tag)` tuples. Only `//`-style line comments and
/// inner `///` / `//!` doc comments are considered; bare `txdoc:` strings
/// outside comments (e.g. inside string literals) are ignored. Wildcard
/// patterns like `txdoc:TXV3-*` (used in prose to describe the family of
/// tags) are skipped — only well-formed tags with at least one segment
/// after `TXV3-` and not ending in `-` are returned.
fn extract_txv3_code_references(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let Some(comment_start) = line.find("//") else {
            continue;
        };
        let comment = &line[comment_start..];
        let mut rest = comment;
        while let Some(start) = rest.find("txdoc:TXV3-") {
            let tag_start = start + "txdoc:".len();
            let tail = &rest[tag_start..];
            let end = tail
                .find(|c: char| {
                    !(c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
                })
                .unwrap_or(tail.len());
            let tag = &tail[..end];
            // Require at least one segment after `TXV3-`. A bare `TXV3-`
            // (trailing dash, e.g. from the wildcard `TXV3-*` used in
            // prose) is not a real reference.
            if !tag.is_empty() && !tag.ends_with('-') && tag.len() > "TXV3-".len() {
                out.push((idx + 1, tag.to_string()));
            }
            rest = &tail[end..];
        }
    }
    out
}

/// Given a set of declared txdoc tags (harvested from `docs/Txv3/`) and
/// a list of `(display_path, source_text)` Rust files, return a finding
/// for every `txdoc:TXV3-*` reference in code comments that does not
/// resolve to a declared tag.
fn lint_txv3_code_references(
    known_tags: &BTreeSet<String>,
    files: &[(&str, &str)],
) -> Vec<String> {
    let mut findings = Vec::new();
    for (display, text) in files {
        for (line_no, tag) in extract_txv3_code_references(text) {
            if !known_tags.contains(&tag) {
                findings.push(format!(
                    "{display}:{line_no}: code references unknown txdoc tag `{tag}` (no declaration found in docs/Txv3/)"
                ));
            }
        }
    }
    findings
}

fn lint_step_no_await(display: &str, text: &str) -> Vec<String> {
    const AWAIT_BYTES: &[u8] = b".await";
    let mut findings = Vec::new();
    let mut depth: i32 = 0;
    let mut step_body_depth: Option<i32> = None;
    let mut awaiting_open_brace = false;

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let cleaned = match trimmed.find("//") {
            Some(pos) => &trimmed[..pos],
            None => trimmed,
        };

        if step_body_depth.is_none() && !awaiting_open_brace && cleaned.contains("fn step(") {
            awaiting_open_brace = true;
        }

        let bytes = cleaned.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'{' {
                depth += 1;
                if awaiting_open_brace {
                    awaiting_open_brace = false;
                    step_body_depth = Some(depth);
                }
                i += 1;
            } else if c == b'}' {
                if let Some(body_depth) = step_body_depth {
                    if depth == body_depth {
                        step_body_depth = None;
                    }
                }
                depth -= 1;
                i += 1;
            } else if step_body_depth.is_some()
                && i + AWAIT_BYTES.len() <= bytes.len()
                && &bytes[i..i + AWAIT_BYTES.len()] == AWAIT_BYTES
            {
                findings.push(format!(
                    "{display}:{line_no}: A-3 violation — `.await` inside `step()` body (STEP-2); return `Yield {{ shape: ... }}` and let the driver compose"
                ));
                i += AWAIT_BYTES.len();
            } else {
                i += 1;
            }
        }
    }
    findings
}

fn lint_file_size(path: &str, display: &str, text: &str) -> Option<String> {
    if !path.ends_with(".rs") {
        return None;
    }
    let lines = text.lines().count();
    if lines <= MAX_AUTHORED_RUST_FILE_LINES {
        return None;
    }
    Some(format!(
        "{display}: authored Rust source file has {lines} lines; split files above {MAX_AUTHORED_RUST_FILE_LINES} lines by responsibility"
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UnusedCheckStep {
    name: &'static str,
    args: Vec<String>,
    skip_reason: Option<String>,
}

fn unused_check_steps(installed: &BTreeSet<String>) -> Vec<UnusedCheckStep> {
    unused_check_steps_for_targets(
        installed,
        [
            (TxTarget::Rv64Qemu, target_triple(TxTarget::Rv64Qemu)),
            (
                TxTarget::Rv64M1DockMock,
                target_triple(TxTarget::Rv64M1DockMock),
            ),
            (TxTarget::La64Qemu, target_triple(TxTarget::La64Qemu)),
        ],
    )
}

fn unused_check_steps_for_targets<I>(
    installed: &BTreeSet<String>,
    targets: I,
) -> Vec<UnusedCheckStep>
where
    I: IntoIterator<Item = (TxTarget, Result<String>)>,
{
    let mut steps = vec![UnusedCheckStep {
        name: "host workspace",
        args: strings(["check", "--workspace"]),
        skip_reason: None,
    }];

    for (target, triple) in targets {
        let name = target.name();
        match triple {
            Ok(triple) if installed.contains(&triple) => steps.push(UnusedCheckStep {
                name,
                args: strings(["check", "-p", target.package(), "--target", &triple]),
                skip_reason: None,
            }),
            Ok(triple) => steps.push(UnusedCheckStep {
                name,
                args: Vec::new(),
                skip_reason: Some(format!("install with `rustup target add {triple}`")),
            }),
            Err(err) => steps.push(UnusedCheckStep {
                name,
                args: Vec::new(),
                skip_reason: Some(err),
            }),
        }
    }

    steps
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

fn run_unused_check(root: &Path, args: &[String]) -> Result<()> {
    let rustflags = unused_rustflags();
    println!("$ RUSTFLAGS={} cargo {}", rustflags, shell_join(args));
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .env("RUSTFLAGS", rustflags)
        .status()
        .map_err(|err| format!("failed to run cargo: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo exited with {status}"))
    }
}

fn unused_rustflags() -> String {
    let unused = "-Dunused";
    match std::env::var("RUSTFLAGS") {
        Ok(current) if current.split_whitespace().any(|flag| flag == unused) => current,
        Ok(current) if !current.trim().is_empty() => format!("{current} {unused}"),
        _ => unused.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::TxTarget;

    #[test]
    fn arch_lint_rejects_generic_kernel_target_cfg() {
        let findings = lint_arch_text(
            "crates/tx-kernel/src/lib.rs",
            "crates/tx-kernel/src/lib.rs",
            "#[cfg(target_arch = \"riscv64\")] fn bad() {}",
        );
        assert!(findings
            .iter()
            .any(|finding| finding.contains("must not cfg on target_arch")));
    }

    #[test]
    fn arch_lint_rejects_runtime_hal_vocabulary() {
        let findings = lint_arch_text(
            "crates/tx-kernel/src/lib.rs",
            "crates/tx-kernel/src/lib.rs",
            "struct HalManager;",
        );
        assert!(findings
            .iter()
            .any(|finding| finding.contains("runtime HAL vocabulary")));
    }

    #[test]
    fn arch_lint_rejects_raw_boot_values_in_generic_kernel() {
        let findings = lint_arch_text(
            "crates/tx-kernel/src/lib.rs",
            "crates/tx-kernel/src/lib.rs",
            "pub fn kernel_main(cpu_id: CpuId, firmware_arg: BootArg) -> ! { loop {} }",
        );
        assert!(findings
            .iter()
            .any(|finding| finding.contains("must consume BootHandoff")));
    }

    #[test]
    fn arch_lint_rejects_upper_raw_zone_policy() {
        let findings = lint_arch_text(
            "crates/tx-kernel/src/lib.rs",
            "crates/tx-kernel/src/lib.rs",
            "type Bad = Zone<Foo, EbrPolicy>;",
        );
        assert!(findings
            .iter()
            .any(|finding| finding.contains("raw Zone<T, Policy>")));
    }

    #[test]
    fn arch_lint_allows_substrate_zone_policy() {
        let findings = lint_arch_text(
            "crates/tx-substrate/src/zone/mod.rs",
            "crates/tx-substrate/src/zone/mod.rs",
            "type Internal = Zone<Foo, EbrPolicy>;",
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn arch_lint_rejects_concrete_board_import_outside_board_boundary() {
        let findings = lint_arch_text(
            "crates/tx-kernel/src/lib.rs",
            "crates/tx-kernel/src/lib.rs",
            "use tx_hal_riscv64_qemu_virt::Platform;",
        );
        assert!(findings
            .iter()
            .any(|finding| finding.contains("concrete RV64 platform")));
    }

    #[test]
    fn arch_lint_allows_concrete_board_import_in_board_binary() {
        let findings = lint_arch_text(
            "boards/tx-kernel-riscv64-qemu-virt/src/main.rs",
            "boards/tx-kernel-riscv64-qemu-virt/src/main.rs",
            "type ActivePlatform = tx_hal_riscv64_qemu_virt::Platform;",
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn arch_lint_allows_m1dock_mock_import_in_board_binary() {
        let findings = lint_arch_text(
            "boards/tx-kernel-riscv64-m1dock-mock/src/main.rs",
            "boards/tx-kernel-riscv64-m1dock-mock/src/main.rs",
            "type ActivePlatform = tx_hal_riscv64_m1dock_mock::Platform;",
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn arch_lint_rejects_rv64_qemu_boot_static_address_leaks() {
        let findings = lint_arch_text(
            "boards/tx-hal-riscv64-qemu-virt/src/pmap.rs",
            "boards/tx-hal-riscv64-qemu-virt/src/pmap.rs",
            r#"
fn root() -> usize {
    BOOTSTRAP_ROOT.0.get() as usize
}

unsafe extern "C" {
    static __kernel_start: u8;
}

fn sym() -> usize {
    core::ptr::addr_of!(__kernel_start) as usize
}
"#,
        );

        assert!(findings
            .iter()
            .any(|finding| finding.contains("boot-static capture")));
    }

    #[test]
    fn arch_lint_rejects_dead_code_or_unused_allowances() {
        let findings = lint_arch_text(
            "boards/tx-hal-riscv64-qemu-virt/src/boot_static.rs",
            "boards/tx-hal-riscv64-qemu-virt/src/boot_static.rs",
            "#[allow(dead_code)]\nfn stale_boot_helper() {}",
        );

        assert!(findings
            .iter()
            .any(|finding| finding.contains("unused/dead-code allowances")));
    }

    #[test]
    fn arch_lint_rejects_cap_processidentity_in_tty_identity() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/tty/structure/identity.rs",
            "crates/tx-subsystems/src/tty/structure/identity.rs",
            "pub struct SessionPgrp { pub session_leader: Cap<ProcessIdentity> }",
        );

        assert!(findings
            .iter()
            .any(|finding| finding.contains("TTY-CTL-1 violation")));
    }

    #[test]
    fn arch_lint_allows_weak_refs_in_tty_identity() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/tty/structure/identity.rs",
            "crates/tx-subsystems/src/tty/structure/identity.rs",
            "pub session: Option<Weak<Session>>,\npub foreground_pgrp: Option<Weak<ProcessGroup>>,",
        );

        assert!(findings
            .iter()
            .all(|finding| !finding.contains("TTY-CTL-1")));
    }

    #[test]
    fn arch_lint_rejects_foreground_pgrp_field_in_process_structure() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/process/structure.rs",
            "crates/tx-subsystems/src/process/structure.rs",
            "pub struct Session { pub foreground_pgrp: AtomicSlot<Option<Weak<ProcessGroup>>> }",
        );

        assert!(findings
            .iter()
            .any(|finding| finding.contains("TTY-CTL-1a violation")));
    }

    #[test]
    fn arch_lint_allows_foreground_pgrp_cap_method_in_process_structure() {
        // The accessor that hides the two-hop dereference is allowed —
        // it's a method, not a field. The lint should distinguish
        // `foreground_pgrp_cap(` from `foreground_pgrp:`.
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/process/structure.rs",
            "crates/tx-subsystems/src/process/structure.rs",
            "pub fn foreground_pgrp_cap(&self) -> Option<Cap<ProcessGroup>> { None }",
        );

        assert!(findings
            .iter()
            .all(|finding| !finding.contains("TTY-CTL-1a")));
    }

    #[test]
    fn arch_lint_allows_foreground_pgrp_in_doc_comments_in_process_structure() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/process/structure.rs",
            "crates/tx-subsystems/src/process/structure.rs",
            "/// The `foreground_pgrp:` field on TtyIdentity.SessionPgrp is the home.",
        );

        assert!(findings
            .iter()
            .all(|finding| !finding.contains("TTY-CTL-1a")));
    }

    #[test]
    fn file_size_lint_rejects_authored_rust_files_over_limit() {
        let text = "fn f() {}\n".repeat(MAX_AUTHORED_RUST_FILE_LINES + 1);
        let finding = lint_file_size(
            "boards/tx-hal-riscv64-qemu-virt/src/pmap/mod.rs",
            "boards/tx-hal-riscv64-qemu-virt/src/pmap/mod.rs",
            &text,
        );

        assert!(finding.is_some_and(|finding| finding.contains("1500")));
    }

    #[test]
    fn file_size_lint_allows_non_rust_files() {
        let text = "# heading\n".repeat(MAX_AUTHORED_RUST_FILE_LINES + 1);
        let finding = lint_file_size("docs/design/01_substrate/HAL_v1.md", "HAL_v1.md", &text);

        assert!(finding.is_none());
    }

    #[test]
    fn unused_lint_plan_checks_workspace_and_installed_targets() {
        let installed = std::collections::BTreeSet::from(["riscv64gc-unknown-none-elf".into()]);
        let steps = unused_check_steps_for_targets(
            &installed,
            [
                (TxTarget::Rv64Qemu, Ok("riscv64gc-unknown-none-elf".into())),
                (
                    TxTarget::La64Qemu,
                    Ok("loongarch64-unknown-none-softfloat".into()),
                ),
            ],
        );

        assert!(steps.iter().any(|step| {
            step.skip_reason.is_none() && step.args == vec!["check", "--workspace"]
        }));
        assert!(steps.iter().any(|step| {
            step.skip_reason.is_none()
                && step
                    .args
                    .contains(&"tx-kernel-riscv64-qemu-virt".to_string())
        }));
        assert!(steps.iter().any(|step| {
            step.skip_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("rustup target add loongarch64"))
        }));
    }

    #[test]
    fn rv64_qemu_linker_separates_low_load_and_high_vma_symbols() {
        let linker = include_str!("../../boards/tx-hal-riscv64-qemu-virt/linker-rv64-qemu-virt.ld");

        assert!(linker.contains("KERNEL_LOAD_BASE = 0x80200000;"));
        assert!(linker.contains("KERNEL_VIRT_BASE = 0xffffffff80200000;"));
        assert!(linker.contains("KERNEL_VIRT_OFFSET = KERNEL_VIRT_BASE - KERNEL_LOAD_BASE;"));
        assert!(linker.contains(".text.trampoline"));
        assert!(linker.contains("__kernel_start = .;"));
        assert!(linker.contains("__kernel_start_load = KERNEL_LOAD_BASE;"));
        assert!(linker.contains("__text_start_load = LOADADDR(.text);"));
        assert!(linker.contains("__bss_start_load = __bss_start - KERNEL_VIRT_OFFSET;"));
        assert!(linker.contains("__tx_boot_stack_top_load"));
        assert!(linker.contains("__bootstrap_root_load"));
        assert!(linker.contains("__kernel_alias_l1_load"));
        assert!(linker.contains("__kernel_alias_l0_tables_load"));
        assert!(linker.contains("__pt_node_pool_load"));
    }

    #[test]
    fn arch_lint_rejects_await_in_step_fn_body() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/foo.rs",
            "crates/tx-subsystems/src/foo.rs",
            r#"
impl StepOp for FooOp {
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
        let _ = something().await;
        StepOutcome::Done(())
    }
}
"#,
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("A-3") && finding.contains(".await")),
            "expected A-3 finding for `.await` inside step body, got {findings:?}"
        );
    }

    #[test]
    fn arch_lint_rejects_await_in_same_line_step_fn_body() {
        // `fn step(...) -> X { body }` on a single line still counts.
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/foo.rs",
            "crates/tx-subsystems/src/foo.rs",
            "fn step(&mut self) -> StepOutcome<(), NoProgress> { let _ = future().await; StepOutcome::Done(()) }",
        );
        assert!(
            findings.iter().any(|finding| finding.contains("A-3")),
            "expected A-3 finding for single-line step body, got {findings:?}"
        );
    }

    #[test]
    fn arch_lint_allows_await_outside_step_fn() {
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/foo.rs",
            "crates/tx-subsystems/src/foo.rs",
            r#"
async fn helper() -> u32 {
    other().await
}

fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<(), NoProgress> {
    StepOutcome::Done(())
}
"#,
        );
        assert!(
            !findings.iter().any(|finding| finding.contains("A-3")),
            "did not expect A-3 finding for `.await` in async helper, got {findings:?}"
        );
    }

    #[test]
    fn arch_lint_allows_step_underscore_named_fn_with_await() {
        // `fn step_helper` is not the trait method `fn step` — only the
        // exact `step` name is the StepOp::step contract. This test
        // pins the name match.
        let findings = lint_arch_text(
            "crates/tx-subsystems/src/foo.rs",
            "crates/tx-subsystems/src/foo.rs",
            r#"
async fn step_helper() -> u32 {
    other().await
}
"#,
        );
        assert!(
            !findings.iter().any(|finding| finding.contains("A-3")),
            "did not expect A-3 finding for fn step_helper, got {findings:?}"
        );
    }

    #[test]
    fn rv64_qemu_trampoline_uses_only_low_load_symbols() {
        // The trampoline `core::arch::global_asm!(...)` block lives in its
        // own file since the 2026-05-08 jumbo-mod split (see
        // `docs/progress/STATUS.md`); the lib.rs `mod boot_trampoline;`
        // declaration is the only place lib.rs touches it.
        let source = include_str!("../../boards/tx-hal-riscv64-qemu-virt/src/boot_trampoline.rs");
        let start = source
            .find(".section .text.trampoline")
            .expect("trampoline section");
        let end = source[start..]
            .find("\n\"#\n);")
            .map(|offset| start + offset)
            .expect("trampoline asm string terminator");
        let trampoline = &source[start..end];

        assert!(!trampoline.contains("call tx_rv64_qemu_prepare_high_boot"));
        for high_symbol in [
            "__kernel_start",
            "__bss_start",
            "__tx_boot_stack_top",
            "__global_pointer$",
            "rust_entry",
        ] {
            for line in trampoline.lines() {
                if line.contains(high_symbol) {
                    assert!(
                        line.contains("_load"),
                        "trampoline line references high symbol `{high_symbol}` without _load: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn docs_lint_accepts_code_reference_to_known_txv3_tag() {
        // A Rust file that mentions `txdoc:TXV3-STEP-MODEL-V2` in a comment
        // must not produce a finding when that tag is declared in some
        // docs/Txv3/ markdown. The harvest is provided as input so the
        // test is hermetic.
        let mut known = BTreeSet::<String>::new();
        known.insert("TXV3-STEP-MODEL-V2".to_string());

        let findings = lint_txv3_code_references(
            &known,
            &[(
                "crates/tx-substrate/src/step_v3.rs",
                "// Implements the v3 step algebra. txdoc:TXV3-STEP-MODEL-V2\npub struct Foo;\n",
            )],
        );

        assert!(
            findings.is_empty(),
            "expected no findings for known TXV3 tag, got {findings:?}"
        );
    }

    #[test]
    fn docs_lint_rejects_code_reference_to_unknown_txv3_tag() {
        // A Rust file mentioning `txdoc:TXV3-DOES-NOT-EXIST` in a comment
        // must produce a finding naming the missing tag, because no
        // docs/Txv3/ markdown declares it.
        let known = BTreeSet::<String>::new();

        let findings = lint_txv3_code_references(
            &known,
            &[(
                "crates/tx-substrate/src/step_v3.rs",
                "// txdoc:TXV3-DOES-NOT-EXIST referenced but no doc declares it\npub struct Foo;\n",
            )],
        );

        assert!(
            findings
                .iter()
                .any(|f| f.contains("TXV3-DOES-NOT-EXIST") && f.contains("step_v3.rs")),
            "expected finding mentioning missing tag and file, got {findings:?}"
        );
    }

    #[test]
    fn docs_lint_treats_code_reference_to_v4_design_tag_as_already_supported_or_explicit_pass() {
        // Pin existing behavior: a Rust file mentioning a non-TXV3 tag
        // (e.g. `txdoc:CONCEPT-V4-OBJECT-MODEL`) must not be flagged by
        // the new TXV3 harvest. Only `txdoc:TXV3-*` references are scanned
        // by the harvest under test; design-doc tags are out of scope here
        // and the existing docs/design/ harvest handles them separately.
        let known = BTreeSet::<String>::new();

        let findings = lint_txv3_code_references(
            &known,
            &[(
                "crates/tx-substrate/src/lib.rs",
                "// txdoc:CONCEPT-V4-OBJECT-MODEL — see docs/design/...\npub struct Foo;\n",
            )],
        );

        assert!(
            findings.is_empty(),
            "non-TXV3 references must not be flagged by the TXV3 harvest, got {findings:?}"
        );
    }
}
