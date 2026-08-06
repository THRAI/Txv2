//! Portable-network red-gate.
//!
//! The positive/negative tests define the portability boundary. A repository
//! scan must stay clean after the Phase 5 migration and is part of fast CI.

use std::fmt;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

const SCAN_ROOTS: &[&str] = &[
    "crates/tx-hal/src",
    "crates/tx-kernel/src",
    "crates/tx-drivers/src",
    "crates/tx-subsystems/src/net",
    "boards",
    "xtask/src",
    "tools",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Rule {
    GenericPlatformBranch,
    SingletonNetIrq,
    ProjectionAsHardwareIdentity,
    FallbackOrFirstDevice,
    OrdinalResourceName,
    DeploymentLiteral,
    FixedQemuPlacement,
    TlsVerificationBypass,
}

impl Rule {
    fn id(self) -> &'static str {
        match self {
            Self::GenericPlatformBranch => "NETPORT-ARCH",
            Self::SingletonNetIrq => "NETPORT-IRQ",
            Self::ProjectionAsHardwareIdentity => "NETPORT-IDENTITY",
            Self::FallbackOrFirstDevice => "NETPORT-FALLBACK",
            Self::OrdinalResourceName => "NETPORT-ORDINAL",
            Self::DeploymentLiteral => "NETPORT-DEPLOYMENT",
            Self::FixedQemuPlacement => "NETPORT-PLACEMENT",
            Self::TlsVerificationBypass => "NETPORT-TLS",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::GenericPlatformBranch => "generic network code branches on architecture/board",
            Self::SingletonNetIrq => "tier-2 network IRQ is platform-global",
            Self::ProjectionAsHardwareIdentity => {
                "namespace interface name is used as hardware identity"
            }
            Self::FallbackOrFirstDevice => "production path guesses a fallback/first device",
            Self::OrdinalResourceName => "driver lookup uses an ordinal/generated resource name",
            Self::DeploymentLiteral => "shared network path embeds a deployment/topology value",
            Self::FixedQemuPlacement => "shared QEMU/board path fixes NIC placement",
            Self::TlsVerificationBypass => "TLS certificate verification is disabled",
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Finding {
    rel: String,
    line: usize,
    rule: Rule,
    snippet: String,
}

pub(crate) fn lint_net_portability(root: &Path) -> Result<()> {
    let findings = scan_repository(root)?;

    println!("Network portability lint");
    println!("========================");
    println!("txdoc:INV-V5-DEVRES findings: {}", findings.len(),);

    for finding in &findings {
        println!(
            "{}:{}: {} {}: {}",
            finding.rel,
            finding.line,
            finding.rule,
            finding.rule.summary(),
            finding.snippet
        );
    }

    if findings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "net-portability lint found {} portability violation(s)",
            findings.len()
        ))
    }
}

fn scan_repository(root: &Path) -> Result<Vec<Finding>> {
    let mut files = Vec::new();
    for root_rel in SCAN_ROOTS {
        let scan_root = root.join(root_rel);
        if scan_root.exists() {
            files.extend(collect_files(&scan_root, &["rs", "sh"]).map_err(|err| err.to_string())?);
        }
    }
    files.sort();
    files.dedup();

    let mut findings = Vec::new();
    for file in files {
        let rel = relative(root, &file).replace('\\', "/");
        if !is_scanned_path(&rel) {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
        findings.extend(scan_text(&rel, &text));
    }
    findings.sort();
    findings.dedup();
    Ok(findings)
}

fn scan_text(rel: &str, text: &str) -> Vec<Finding> {
    if !is_scanned_path(rel) {
        return Vec::new();
    }

    let lines: Vec<&str> = text.lines().collect();
    let test_cutoff = rel
        .ends_with(".rs")
        .then(|| rust_test_module_cutoff(&lines))
        .flatten();
    let mut findings = Vec::new();
    let mut in_block_comment = false;

    for (idx, line) in lines.iter().enumerate() {
        if test_cutoff.is_some_and(|cutoff| idx >= cutoff) {
            break;
        }
        let Some(code) = code_before_comment(line, rel.ends_with(".sh"), &mut in_block_comment)
        else {
            continue;
        };
        let line_no = idx + 1;

        if is_generic_network_path(rel)
            && (code.contains("P::ARCH")
                || code.contains("match P::ARCH")
                || code.contains("cfg(target_arch")
                || code.contains("cfg!(target_arch"))
        {
            push(
                &mut findings,
                rel,
                line_no,
                Rule::GenericPlatformBranch,
                code,
            );
        }

        if is_kernel_or_board_rust(rel)
            && (contains_ident(code, "NET_IRQ") || code.contains("net_irq("))
        {
            push(&mut findings, rel, line_no, Rule::SingletonNetIrq, code);
        }

        if is_generic_hardware_path(rel)
            && ((code.contains("net_device_by_name") && code.contains("eth0"))
                || code.contains("b\"eth0\"")
                || code.contains("\"eth0\""))
        {
            push(
                &mut findings,
                rel,
                line_no,
                Rule::ProjectionAsHardwareIdentity,
                code,
            );
        }

        if is_generic_device_selection_path(rel)
            && (code.contains("VIRTIO_NET0_")
                || code.contains("boot_net_registration")
                || code.contains("devices.is_empty()")
                || code.contains("net_device_snapshot().into_iter().next()")
                || code.contains(".first()")
                || (code.contains("unwrap_or") && code.contains("REGISTRATION")))
        {
            push(
                &mut findings,
                rel,
                line_no,
                Rule::FallbackOrFirstDevice,
                code,
            );
        }

        if is_generic_driver_or_device_path(rel)
            && ["\"virtio0\"", "\"pcie-ecam\"", "\"pcie-mmio32\""]
                .iter()
                .any(|term| code.contains(term))
        {
            push(&mut findings, rel, line_no, Rule::OrdinalResourceName, code);
        }

        if is_shared_network_configuration_path(rel) && has_deployment_literal(code) {
            push(&mut findings, rel, line_no, Rule::DeploymentLiteral, code);
        }

        if has_fixed_qemu_placement(rel, code) {
            push(&mut findings, rel, line_no, Rule::FixedQemuPlacement, code);
        }

        if is_network_tool(rel) && has_tls_bypass(code) {
            push(
                &mut findings,
                rel,
                line_no,
                Rule::TlsVerificationBypass,
                code,
            );
        }
    }

    findings
}

fn push(findings: &mut Vec<Finding>, rel: &str, line: usize, rule: Rule, code: &str) {
    findings.push(Finding {
        rel: rel.to_string(),
        line,
        rule,
        snippet: code.trim().chars().take(180).collect(),
    });
}

fn is_scanned_path(rel: &str) -> bool {
    if is_explicit_fixture_or_capture(rel)
        || rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
        || rel.ends_with("_tests.rs")
    {
        return false;
    }

    rel.starts_with("crates/tx-hal/src/")
        || rel.starts_with("crates/tx-kernel/src/")
        || rel.starts_with("crates/tx-drivers/src/")
        || rel.starts_with("crates/tx-subsystems/src/net/")
        || rel.starts_with("boards/")
        || matches!(rel, "xtask/src/qemu.rs" | "xtask/src/shell_test.rs")
        || is_network_tool(rel)
}

fn is_explicit_fixture_or_capture(rel: &str) -> bool {
    rel.starts_with("tools/network-scenarios/")
        || rel.contains("/fixtures/")
        || rel.contains("/captures/")
        || rel.contains("/testdata/")
}

fn is_generic_network_path(rel: &str) -> bool {
    rel == "crates/tx-kernel/src/devices.rs"
        || rel == "crates/tx-kernel/src/irq.rs"
        || rel == "crates/tx-kernel/src/init/net.rs"
        || rel.starts_with("crates/tx-drivers/src/")
        || rel.starts_with("crates/tx-subsystems/src/net/")
}

fn is_kernel_or_board_rust(rel: &str) -> bool {
    rel.ends_with(".rs") && (rel.starts_with("crates/") || rel.starts_with("boards/"))
}

fn is_generic_hardware_path(rel: &str) -> bool {
    matches!(
        rel,
        "crates/tx-kernel/src/devices.rs"
            | "crates/tx-kernel/src/irq.rs"
            | "crates/tx-kernel/src/init/net.rs"
    ) || rel.starts_with("crates/tx-drivers/src/")
}

fn is_generic_device_selection_path(rel: &str) -> bool {
    matches!(
        rel,
        "crates/tx-kernel/src/devices.rs" | "crates/tx-kernel/src/init/net.rs"
    )
}

fn is_generic_driver_or_device_path(rel: &str) -> bool {
    rel == "crates/tx-kernel/src/devices.rs" || rel.starts_with("crates/tx-drivers/src/")
}

fn is_shared_network_configuration_path(rel: &str) -> bool {
    rel == "crates/tx-kernel/src/init/net.rs"
        || matches!(rel, "xtask/src/qemu.rs" | "xtask/src/shell_test.rs")
        || is_network_tool(rel)
}

fn is_network_tool(rel: &str) -> bool {
    if !rel.starts_with("tools/") || !rel.ends_with(".sh") {
        return false;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    ["net", "git", "dhcp", "udhcpc", "qemu", "shell"]
        .iter()
        .any(|term| name.contains(term))
}

fn has_deployment_literal(code: &str) -> bool {
    contains_ipv4_literal(code)
        || code.contains("Ipv4Address::new([")
        || code.contains("TX_DHCP_QEMU_NET:-")
        || code.contains("TX_DHCP_QEMU_DHCPSTART:-")
        || code.contains("https://")
        || code.contains("http://")
}

fn has_fixed_qemu_placement(rel: &str, code: &str) -> bool {
    let shared_renderer =
        matches!(rel, "xtask/src/qemu.rs" | "xtask/src/shell_test.rs") || is_network_tool(rel);
    (shared_renderer
        && code.contains("virtio-net")
        && (code.contains("virtio-mmio-bus.") || code.contains("addr=")))
        || (rel.starts_with("boards/")
            && (code.contains("VIRTIO_NET_PCI_SLOT") || code.contains("VIRTIO_NET_PCI_FUNCTION")))
}

fn has_tls_bypass(code: &str) -> bool {
    code.contains("GIT_SSL_NO_VERIFY")
        || code.contains("http.sslVerify=false")
        || code.contains("http.sslVerify false")
        || code.contains("--insecure")
        || code.contains("curl -k ")
}

fn contains_ipv4_literal(code: &str) -> bool {
    let bytes = code.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if !bytes[idx].is_ascii_digit() {
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == b'.') {
            idx += 1;
        }
        let candidate = &code[start..idx];
        let octets: Vec<&str> = candidate.split('.').collect();
        if octets.len() == 4
            && octets
                .iter()
                .all(|octet| !octet.is_empty() && octet.len() <= 3 && octet.parse::<u8>().is_ok())
        {
            return true;
        }
    }
    false
}

fn rust_test_module_cutoff(lines: &[&str]) -> Option<usize> {
    for (idx, line) in lines.iter().enumerate() {
        if !line.contains("cfg(test)") && !line.contains("cfg(all(test") {
            continue;
        }
        if lines
            .iter()
            .skip(idx + 1)
            .take(4)
            .map(|line| line.trim())
            .any(|line| line.starts_with("mod tests"))
        {
            return Some(idx);
        }
    }
    None
}

fn code_before_comment<'a>(
    line: &'a str,
    shell: bool,
    in_block_comment: &mut bool,
) -> Option<&'a str> {
    let trimmed = line.trim_start();
    if *in_block_comment {
        if trimmed.contains("*/") {
            *in_block_comment = false;
        }
        return None;
    }
    if trimmed.starts_with("/*") {
        if !trimmed.contains("*/") {
            *in_block_comment = true;
        }
        return None;
    }
    if trimmed.is_empty() || trimmed.starts_with("//") || (shell && trimmed.starts_with('#')) {
        return None;
    }

    let bytes = line.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        if byte == b'\\' && quote.is_some() {
            escaped = true;
            idx += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
            idx += 1;
            continue;
        }
        if quote.is_none() {
            if !shell && byte == b'/' && bytes.get(idx + 1) == Some(&b'/') {
                return nonempty(&line[..idx]);
            }
            if shell && byte == b'#' {
                return nonempty(&line[..idx]);
            }
        }
        idx += 1;
    }
    nonempty(line)
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
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
        let before = start.checked_sub(1).and_then(|idx| bytes.get(idx)).copied();
        let after = bytes.get(start + needle.len()).copied();
        if !before.is_some_and(is_ident_byte) && !after.is_some_and(is_ident_byte) {
            return true;
        }
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::{scan_text, Rule};

    #[test]
    fn negative_snippets_cover_every_rule() {
        let cases = [
            (
                "crates/tx-kernel/src/devices.rs",
                "fn bind<P: TxPlatform>() { match P::ARCH { _ => {} } }",
                Rule::GenericPlatformBranch,
            ),
            (
                "crates/tx-hal/src/lib.rs",
                "const NET_IRQ: u32 = 2; fn net_irq() -> u32 { NET_IRQ }",
                Rule::SingletonNetIrq,
            ),
            (
                "crates/tx-kernel/src/irq.rs",
                "let nic = net_device_by_name(b\"eth0\");",
                Rule::ProjectionAsHardwareIdentity,
            ),
            (
                "crates/tx-kernel/src/init/net.rs",
                "snapshot.into_iter().next().unwrap_or(&VIRTIO_NET0_REGISTRATION);",
                Rule::FallbackOrFirstDevice,
            ),
            (
                "crates/tx-kernel/src/devices.rs",
                "let net = VirtioNet::new(\"virtio0\");",
                Rule::OrdinalResourceName,
            ),
            (
                "tools/verify-git-net.sh",
                "echo 'nameserver 10.0.2.3' > /etc/resolv.conf",
                Rule::DeploymentLiteral,
            ),
            (
                "xtask/src/qemu.rs",
                "args.push(\"virtio-net-pci,netdev=net0,addr=2\".into());",
                Rule::FixedQemuPlacement,
            ),
            (
                "tools/verify-git-net.sh",
                "export GIT_SSL_NO_VERIFY=true",
                Rule::TlsVerificationBypass,
            ),
        ];

        for (rel, source, expected) in cases {
            let findings = scan_text(rel, source);
            assert!(
                findings.iter().any(|finding| finding.rule == expected),
                "{rel} did not report {}: {findings:?}",
                expected.id()
            );
        }
    }

    #[test]
    fn fixtures_protocol_constants_and_scenario_reads_are_allowed() {
        assert!(scan_text(
            "tools/network-scenarios/v1/fixtures/relocated.sh",
            "NET=172.31.44.0/24; DEVICE='virtio-net-pci,addr=9'",
        )
        .is_empty());
        assert!(scan_text(
            "crates/tx-drivers/src/dwmac/registers.rs",
            "const DMA_STATUS_TI: u32 = 1 << 0; const ETH_P_ARP: u16 = 0x0806;",
        )
        .is_empty());
        assert!(scan_text(
            "xtask/src/qemu.rs",
            "args.extend(scenario.render_qemu_network(target)?);",
        )
        .is_empty());
        assert!(scan_text(
            "tools/udhcpc-guest-probe.sh",
            "dns_server=\"$(awk '/^nameserver / { print $2; exit }' /etc/resolv.conf)\"",
        )
        .is_empty());
    }

    #[test]
    fn comments_and_inline_test_module_do_not_create_findings() {
        let source = r#"
// const NET_IRQ: u32 = 2;
fn production(route: &IrqRoute) { route.enable(); }
#[cfg(test)]
mod tests {
    const NET_IRQ: u32 = 2;
    fn fixture() { let _ = net_device_by_name(b"eth0"); }
}
"#;
        assert!(scan_text("crates/tx-kernel/src/irq.rs", source).is_empty());
    }

    #[test]
    fn findings_are_deterministic_after_repository_sort_key() {
        let mut findings = scan_text(
            "crates/tx-kernel/src/init/net.rs",
            "const BOOT: Ipv4Address = Ipv4Address::new([10, 0, 2, 15]);\nlet _ = b\"eth0\";",
        );
        findings.sort();
        assert!(findings.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
