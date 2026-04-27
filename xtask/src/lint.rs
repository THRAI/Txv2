use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::{collect_files, relative};
use crate::Result;

pub(crate) fn lint(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("lint command needs `arch` or `docs`".into());
    };
    match kind.as_str() {
        "arch" => lint_arch(root),
        "docs" => lint_docs(root),
        other => Err(format!(
            "unknown lint kind '{other}', expected arch or docs"
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

fn lint_arch_text(path: &str, display: &str, text: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
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
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
