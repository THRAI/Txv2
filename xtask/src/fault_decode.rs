use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use addr2line::Loader;
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::target::TxTarget;
use crate::util::{optional_option_value, resolve_path};
use crate::Result;

const RV64_KERNEL_WINDOW_SIZE: u64 = 512 * 1024 * 1024;
const RV64_USER_TOP: u64 = 0x0000_0040_0000_0000;
const RV64_SV39_BITS: u32 = 39;
const RV64_REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

pub(crate) fn fault_decode(root: &Path, args: Vec<String>) -> Result<()> {
    let config = FaultDecodeConfig::parse(root, &args)?;
    let spec = TargetSpec::for_target(config.target, root)?;
    let elf_path = config.elf.unwrap_or_else(|| spec.elf_path.clone());
    let image = ElfImage::load(&elf_path, &spec)?;

    println!("txKernel fault-decode: {}", spec.name);
    println!("ELF: {}", elf_path.display());
    println!("ELF layout: {}", image.layout.label());
    println!();

    match config.input {
        FaultDecodeInput::Address(addr) => {
            print_address_block("address", addr, &image, &spec);
        }
        FaultDecodeInput::Trap(trap) => {
            print_trap_block(1, &trap, &image, &spec);
        }
        FaultDecodeInput::Serial { path, all } => {
            let serial = fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            let traps = parse_traps(&serial);
            if traps.is_empty() {
                return Err(format!(
                    "no scause/sepc/stval trap lines found in {}",
                    path.display()
                ));
            }
            let selected = if all {
                traps
            } else {
                vec![traps.last().expect("checked non-empty").clone()]
            };
            for (index, trap) in selected.iter().enumerate() {
                print_trap_block(index + 1, trap, &image, &spec);
                if index + 1 != selected.len() {
                    println!();
                }
            }
        }
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct FaultDecodeConfig {
    target: TxTarget,
    elf: Option<PathBuf>,
    input: FaultDecodeInput,
}

#[derive(Clone, Debug)]
enum FaultDecodeInput {
    Serial { path: PathBuf, all: bool },
    Trap(TrapRecord),
    Address(u64),
}

impl FaultDecodeConfig {
    fn parse(root: &Path, args: &[String]) -> Result<Self> {
        let target = optional_option_value(args, "--target")
            .ok_or_else(|| "missing required option --target".to_string())
            .and_then(|value| TxTarget::parse(&value))?;
        let elf = optional_option_value(args, "--elf").map(|path| resolve_path(root, path.into()));
        let all = args.iter().any(|arg| arg == "--all");

        let input = if let Some(path) = optional_option_value(args, "--serial") {
            FaultDecodeInput::Serial {
                path: resolve_path(root, path.into()),
                all,
            }
        } else if let Some(addr) = optional_option_value(args, "--addr") {
            FaultDecodeInput::Address(parse_u64_value(&addr)?)
        } else {
            let scause = optional_option_value(args, "--scause");
            let sepc = optional_option_value(args, "--sepc");
            let stval = optional_option_value(args, "--stval");
            match (scause, sepc, stval) {
                (Some(scause), Some(sepc), Some(stval)) => FaultDecodeInput::Trap(TrapRecord {
                    scause: parse_u64_value(&scause)?,
                    sepc: parse_u64_value(&sepc)?,
                    stval: parse_u64_value(&stval)?,
                    frame: None,
                }),
                (None, None, None) => {
                    return Err("provide --serial, --addr, or --scause/--sepc/--stval".into())
                }
                _ => {
                    return Err(
                        "explicit trap input requires --scause HEX --sepc HEX --stval HEX".into(),
                    )
                }
            }
        };

        Ok(Self { target, elf, input })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrapRecord {
    scause: u64,
    sepc: u64,
    stval: u64,
    frame: Option<Box<TrapFrameDump>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TrapFrameDump {
    x: [u64; 32],
    scause: u64,
    sepc: u64,
    stval: u64,
    sstatus: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrapKind {
    SynchronousException,
    Interrupt,
}

impl TrapKind {
    fn label(self) -> &'static str {
        match self {
            Self::SynchronousException => "synchronous exception",
            Self::Interrupt => "interrupt",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScauseInfo {
    raw: u64,
    kind: TrapKind,
    code: u64,
    name: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelLinkMode {
    LowLinkedHighAlias {
        elf_base: u64,
        high_alias_base: u64,
    },
    HighLinkedLowLoaded {
        kernel_virt_base: u64,
        kernel_phys_base: u64,
    },
}

impl KernelLinkMode {
    fn label(self) -> &'static str {
        match self {
            Self::LowLinkedHighAlias { .. } => "low-linked/high-alias",
            Self::HighLinkedLowLoaded { .. } => "high-VMA/low-LMA",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimeClass {
    DirectMap {
        phys: u64,
        region: Option<&'static str>,
    },
    HighKernelAlias,
    LowKernelAddress,
    UserAddress,
    SuspiciousNonCanonical,
    UnknownHighRuntime,
    Other,
}

impl RuntimeClass {
    fn label(&self) -> &'static str {
        match self {
            Self::DirectMap { .. } => "direct-map",
            Self::HighKernelAlias => "high-kernel-alias",
            Self::LowKernelAddress => "low-kernel-address",
            Self::UserAddress => "user-address",
            Self::SuspiciousNonCanonical => "suspicious non-canonical or widened address",
            Self::UnknownHighRuntime => "unknown high runtime range",
            Self::Other => "unclassified",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AddressCandidate {
    label: &'static str,
    address: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SymbolInfo {
    name: String,
    address: u64,
    size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TraceEntry {
    offset: i64,
    word: u64,
    confidence: &'static str,
}

#[derive(Clone, Debug)]
struct TargetSpec {
    name: &'static str,
    elf_path: PathBuf,
    kernel_phys_base: u64,
    kernel_virt_base: u64,
    direct_map_base: u64,
    dram_base: u64,
    firmware_gap: Range<u64>,
}

impl TargetSpec {
    fn for_target(target: TxTarget, root: &Path) -> Result<Self> {
        match target {
            TxTarget::Rv64Qemu => Ok(Self {
                name: target.name(),
                elf_path: target.kernel_path(root),
                kernel_phys_base: 0x8020_0000,
                kernel_virt_base: 0xffff_ffff_8020_0000,
                direct_map_base: 0xffff_ffc0_0000_0000,
                dram_base: 0x8000_0000,
                firmware_gap: 0x8000_0000..0x8020_0000,
            }),
            other => Err(format!(
                "fault-decode MVP supports rv64-qemu only, got {}",
                other.name()
            )),
        }
    }

    #[cfg(test)]
    fn rv64_qemu() -> Self {
        Self {
            name: "rv64-qemu",
            elf_path: PathBuf::from(
                "target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt",
            ),
            kernel_phys_base: 0x8020_0000,
            kernel_virt_base: 0xffff_ffff_8020_0000,
            direct_map_base: 0xffff_ffc0_0000_0000,
            dram_base: 0x8000_0000,
            firmware_gap: 0x8000_0000..0x8020_0000,
        }
    }
}

#[derive(Clone, Debug)]
struct SectionInfo {
    name: String,
    address: u64,
    size: u64,
    kind: SectionKind,
    data: Option<Vec<u8>>,
}

impl SectionInfo {
    fn end(&self) -> u64 {
        self.address.saturating_add(self.size)
    }

    fn contains(&self, addr: u64) -> bool {
        self.size != 0 && (self.address..self.end()).contains(&addr)
    }

    fn is_text(&self) -> bool {
        self.kind == SectionKind::Text || self.name.starts_with(".text")
    }

    fn is_static_data(&self) -> bool {
        matches!(self.kind, SectionKind::Data | SectionKind::ReadOnlyData)
            || self.name.starts_with(".data")
            || self.name.starts_with(".rodata")
    }
}

#[derive(Clone, Debug)]
struct FrameInfo {
    function: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

#[derive(Clone, Debug)]
struct AddressAnalysis {
    raw: u64,
    runtime_class: RuntimeClass,
    candidates: Vec<AddressCandidate>,
    selected: Option<AddressCandidate>,
    confidence: &'static str,
    section_name: Option<String>,
    frames: Vec<FrameInfo>,
    nearest: Option<(String, u64)>,
    data_trace: Vec<TraceEntry>,
    data_note: Option<&'static str>,
}

struct ElfImage {
    layout: KernelLinkMode,
    loader: Option<Loader>,
    sections: Vec<SectionInfo>,
    symbols: Vec<SymbolInfo>,
}

impl ElfImage {
    fn load(path: &Path, spec: &TargetSpec) -> Result<Self> {
        let bytes =
            fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let file = object::File::parse(&*bytes)
            .map_err(|err| format!("failed to parse ELF {}: {err}", path.display()))?;

        let mut sections = Vec::new();
        for section in file.sections() {
            let name = section
                .name()
                .unwrap_or("<invalid-section-name>")
                .to_string();
            let kind = section.kind();
            let data = if kind == SectionKind::UninitializedData {
                None
            } else {
                section.data().ok().map(|data| data.to_vec())
            };
            sections.push(SectionInfo {
                name,
                address: section.address(),
                size: section.size(),
                kind,
                data,
            });
        }

        let mut symbols = Vec::new();
        for symbol in file.symbols() {
            if !symbol.is_definition() || symbol.address() == 0 {
                continue;
            }
            if !matches!(
                symbol.kind(),
                SymbolKind::Text | SymbolKind::Label | SymbolKind::Data | SymbolKind::Unknown
            ) {
                continue;
            }
            let Ok(name) = symbol.name() else {
                continue;
            };
            symbols.push(SymbolInfo {
                name: demangle_symbol(name),
                address: symbol.address(),
                size: symbol.size(),
            });
        }
        symbols.sort_by_key(|symbol| symbol.address);

        let layout =
            detect_link_mode(&symbols, spec).unwrap_or(KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            });
        let loader = Loader::new(path).ok();

        Ok(Self {
            layout,
            loader,
            sections,
            symbols,
        })
    }

    fn section_for(&self, addr: u64) -> Option<&SectionInfo> {
        self.sections.iter().find(|section| section.contains(addr))
    }

    fn text_ranges(&self) -> Vec<Range<u64>> {
        self.sections
            .iter()
            .filter(|section| section.is_text())
            .map(|section| section.address..section.end())
            .collect()
    }

    fn analyze_address(&self, raw: u64, spec: &TargetSpec) -> AddressAnalysis {
        let candidates = address_candidates(raw, self.layout, spec);
        let selected = self.select_candidate(&candidates).cloned();
        let selected_addr = selected.as_ref().map(|candidate| candidate.address);
        let selected_section = selected_addr.and_then(|addr| self.section_for(addr));
        let confidence = match (selected.as_ref(), selected_section) {
            (Some(candidate), Some(section))
                if section.is_text() && candidate.label == "raw address" =>
            {
                "direct-code"
            }
            (Some(_), Some(section)) if section.is_text() => "normalized-code-pointer",
            _ => "unresolved",
        };

        let frames = selected_addr
            .map(|addr| self.frames_for(addr))
            .unwrap_or_default();
        let nearest = if selected_section.is_some() {
            selected_addr.and_then(|addr| nearest_symbol(&self.symbols, addr))
        } else {
            None
        };
        let (data_trace, data_note) = selected_addr
            .map(|addr| self.trace_data_candidates(addr, spec))
            .unwrap_or_default();

        AddressAnalysis {
            raw,
            runtime_class: classify_runtime_address(raw, spec),
            candidates,
            selected,
            confidence,
            section_name: selected_section.map(|section| section.name.clone()),
            frames,
            nearest,
            data_trace,
            data_note,
        }
    }

    fn select_candidate<'a>(
        &self,
        candidates: &'a [AddressCandidate],
    ) -> Option<&'a AddressCandidate> {
        candidates
            .iter()
            .find(|candidate| {
                self.section_for(candidate.address)
                    .is_some_and(|section| section.is_text())
            })
            .or_else(|| {
                candidates
                    .iter()
                    .find(|candidate| self.section_for(candidate.address).is_some())
            })
            .or_else(|| candidates.first())
    }

    fn frames_for(&self, addr: u64) -> Vec<FrameInfo> {
        let Some(loader) = &self.loader else {
            return Vec::new();
        };
        let Ok(mut frames) = loader.find_frames(addr) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        loop {
            let frame = match frames.next() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => return out,
            };
            let function = frame
                .function
                .and_then(|function| function.demangle().ok().map(|name| name.into_owned()));
            let (file, line, column) = if let Some(location) = frame.location {
                (
                    location.file.map(str::to_string),
                    location.line,
                    location.column,
                )
            } else {
                (None, None, None)
            };
            out.push(FrameInfo {
                function,
                file,
                line,
                column,
            });
        }
        out
    }

    fn trace_data_candidates(
        &self,
        addr: u64,
        spec: &TargetSpec,
    ) -> (Vec<TraceEntry>, Option<&'static str>) {
        let Some(section) = self.section_for(addr) else {
            return (Vec::new(), None);
        };
        if section.kind == SectionKind::UninitializedData || section.name.starts_with(".bss") {
            return (
                Vec::new(),
                Some("no file contents available for static scan"),
            );
        }
        if !section.is_static_data() {
            return (Vec::new(), None);
        }
        let Some(data) = &section.data else {
            return (
                Vec::new(),
                Some("no file contents available for static scan"),
            );
        };
        (
            scan_data_code_pointers(
                addr,
                section.address,
                data,
                self.layout,
                spec,
                &self.text_ranges(),
            ),
            None,
        )
    }
}

fn parse_traps(serial: &str) -> Vec<TrapRecord> {
    let lines = serial.lines().collect::<Vec<_>>();
    let mut traps = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let Some(mut trap) = parse_trap_summary(lines[index]) else {
            index += 1;
            continue;
        };

        let mut next = index + 1;
        while next < lines.len() && lines[next].trim().is_empty() {
            next += 1;
        }

        if next < lines.len() && lines[next].trim() == "trapframe:" {
            let (frame, consumed) = parse_trapframe_block(&lines[next + 1..]);
            trap.frame = frame;
            index = next + 1 + consumed;
        } else {
            index += 1;
        }

        traps.push(trap);
    }

    traps
}

fn parse_trap_summary(line: &str) -> Option<TrapRecord> {
    Some(TrapRecord {
        scause: parse_value_after_key(line, "scause")?,
        sepc: parse_value_after_key(line, "sepc")?,
        stval: parse_value_after_key(line, "stval")?,
        frame: None,
    })
}

fn parse_trapframe_block(lines: &[&str]) -> (Option<Box<TrapFrameDump>>, usize) {
    let mut regs = [None; 32];
    let mut scause = None;
    let mut sepc = None;
    let mut stval = None;
    let mut sstatus = None;
    let mut consumed = 0;

    for line in lines {
        if line.trim().is_empty() || !(line.starts_with(' ') || line.starts_with('\t')) {
            break;
        }
        consumed += 1;

        for (reg, slot) in regs.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = parse_value_after_key(line, &format!("x{reg}"));
            }
        }
        scause = scause.or_else(|| parse_value_after_key(line, "scause"));
        sepc = sepc.or_else(|| parse_value_after_key(line, "sepc"));
        stval = stval.or_else(|| parse_value_after_key(line, "stval"));
        sstatus = sstatus.or_else(|| parse_value_after_key(line, "sstatus"));
    }

    let frame = match (
        regs.iter().all(Option::is_some),
        scause,
        sepc,
        stval,
        sstatus,
    ) {
        (true, Some(scause), Some(sepc), Some(stval), Some(sstatus)) => {
            Some(Box::new(TrapFrameDump {
                x: regs.map(|reg| reg.expect("checked complete")),
                scause,
                sepc,
                stval,
                sstatus,
            }))
        }
        _ => None,
    };

    (frame, consumed)
}

fn parse_value_after_key(line: &str, key: &str) -> Option<u64> {
    line.split(|ch: char| ch.is_ascii_whitespace() || ch == ',' || ch == ';')
        .filter_map(|token| token.split_once('='))
        .find_map(|(candidate_key, value)| {
            (candidate_key == key)
                .then(|| parse_u64_value(value).ok())
                .flatten()
        })
}

fn parse_u64_value(value: &str) -> Result<u64> {
    let value = value.trim().replace('_', "");
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|err| format!("invalid hex value '{value}': {err}"))
    } else {
        value
            .parse::<u64>()
            .map_err(|err| format!("invalid integer value '{value}': {err}"))
    }
}

fn decode_scause(raw: u64) -> ScauseInfo {
    let interrupt_bit = 1u64 << 63;
    let kind = if raw & interrupt_bit != 0 {
        TrapKind::Interrupt
    } else {
        TrapKind::SynchronousException
    };
    let code = raw & !interrupt_bit;
    let name = match kind {
        TrapKind::SynchronousException => match code {
            0 => "instruction address misaligned",
            1 => "instruction access fault",
            2 => "illegal instruction",
            3 => "breakpoint",
            4 => "load address misaligned",
            5 => "load access fault",
            6 => "store/AMO address misaligned",
            7 => "store/AMO access fault",
            8 => "user environment call",
            9 => "supervisor environment call",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store/AMO page fault",
            _ => "unknown synchronous exception",
        },
        TrapKind::Interrupt => match code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "unknown interrupt",
        },
    };

    ScauseInfo {
        raw,
        kind,
        code,
        name,
    }
}

fn stval_interpretation(scause: &ScauseInfo) -> &'static str {
    if scause.kind == TrapKind::Interrupt {
        return "not defined for interrupts";
    }
    match scause.code {
        1 => "likely faulting instruction access address",
        5 => "likely faulting load access address",
        7 => "likely faulting store/AMO access address",
        12 => "likely faulting instruction page address",
        13 => "likely faulting load page address",
        15 => "likely faulting store/AMO page address",
        2 => "may contain illegal instruction bits",
        _ => "cause-dependent trap value",
    }
}

fn detect_link_mode(symbols: &[SymbolInfo], spec: &TargetSpec) -> Option<KernelLinkMode> {
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.name == "__kernel_start")
        .or_else(|| symbols.iter().find(|symbol| symbol.name == "rust_entry"))?;
    if in_low_kernel_window(symbol.address, spec) {
        Some(KernelLinkMode::LowLinkedHighAlias {
            elf_base: spec.kernel_phys_base,
            high_alias_base: spec.kernel_virt_base,
        })
    } else if in_high_kernel_window(symbol.address, spec) {
        Some(KernelLinkMode::HighLinkedLowLoaded {
            kernel_virt_base: spec.kernel_virt_base,
            kernel_phys_base: spec.kernel_phys_base,
        })
    } else {
        None
    }
}

fn classify_runtime_address(addr: u64, spec: &TargetSpec) -> RuntimeClass {
    if in_high_kernel_window(addr, spec) {
        return RuntimeClass::HighKernelAlias;
    }
    if in_low_kernel_window(addr, spec) {
        return RuntimeClass::LowKernelAddress;
    }
    if addr >= spec.direct_map_base {
        let phys = addr - spec.direct_map_base;
        let region = if spec.firmware_gap.contains(&phys) {
            Some("reserved firmware/loader gap")
        } else if phys >= spec.dram_base {
            Some("DRAM")
        } else {
            None
        };
        return RuntimeClass::DirectMap { phys, region };
    }
    if addr < RV64_USER_TOP {
        return RuntimeClass::UserAddress;
    }
    if !is_sv39_canonical(addr) {
        return RuntimeClass::SuspiciousNonCanonical;
    }
    if addr >> 63 == 1 {
        RuntimeClass::UnknownHighRuntime
    } else {
        RuntimeClass::Other
    }
}

fn address_candidates(addr: u64, mode: KernelLinkMode, spec: &TargetSpec) -> Vec<AddressCandidate> {
    let mut out = Vec::new();
    push_candidate(&mut out, "raw address", addr);

    match mode {
        KernelLinkMode::LowLinkedHighAlias {
            elf_base,
            high_alias_base,
        } => {
            if (high_alias_base..high_alias_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr) {
                let offset = addr - high_alias_base;
                push_candidate(&mut out, "low-linked ELF candidate", elf_base + offset);
            }
            if (elf_base..elf_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr) {
                let offset = addr - elf_base;
                push_candidate(
                    &mut out,
                    "high-linked ELF candidate",
                    high_alias_base + offset,
                );
            }
        }
        KernelLinkMode::HighLinkedLowLoaded {
            kernel_virt_base,
            kernel_phys_base,
        } => {
            if (kernel_phys_base..kernel_phys_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr) {
                let offset = addr - kernel_phys_base;
                push_candidate(
                    &mut out,
                    "high-linked ELF candidate",
                    kernel_virt_base + offset,
                );
            }
            if (kernel_virt_base..kernel_virt_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr) {
                let offset = addr - kernel_virt_base;
                push_candidate(
                    &mut out,
                    "low-linked ELF candidate",
                    kernel_phys_base + offset,
                );
            }
        }
    }

    if !is_sv39_canonical(addr) {
        push_candidate(&mut out, "low 32 bits", addr & 0xffff_ffff);
        push_candidate(&mut out, "Sv39 canonical-like form", sv39_sign_extend(addr));
        push_candidate(
            &mut out,
            "high-alias-like form",
            spec.kernel_virt_base + (addr & (RV64_KERNEL_WINDOW_SIZE - 1)),
        );
    }

    out
}

fn push_candidate(out: &mut Vec<AddressCandidate>, label: &'static str, address: u64) {
    if !out
        .iter()
        .any(|candidate| candidate.label == label && candidate.address == address)
    {
        out.push(AddressCandidate { label, address });
    }
}

fn nearest_symbol(symbols: &[SymbolInfo], addr: u64) -> Option<(String, u64)> {
    symbols
        .iter()
        .filter(|symbol| symbol.address <= addr)
        .max_by_key(|symbol| symbol.address)
        .map(|symbol| (symbol.name.clone(), addr - symbol.address))
}

fn scan_data_code_pointers(
    data_addr: u64,
    section_addr: u64,
    section_data: &[u8],
    mode: KernelLinkMode,
    spec: &TargetSpec,
    text_ranges: &[Range<u64>],
) -> Vec<TraceEntry> {
    if section_data.len() < 8 || data_addr < section_addr {
        return Vec::new();
    }
    let target_offset = (data_addr - section_addr) as usize;
    let start = target_offset.saturating_sub(32) & !7;
    let end = section_data
        .len()
        .min(target_offset.saturating_add(40).saturating_add(7) & !7);
    let mut entries = Vec::new();

    for offset in (start..end).step_by(8) {
        let Some(bytes) = section_data.get(offset..offset + 8) else {
            continue;
        };
        let word = u64::from_le_bytes(bytes.try_into().expect("slice is eight bytes"));
        if !address_candidates(word, mode, spec)
            .iter()
            .any(|candidate| {
                text_ranges
                    .iter()
                    .any(|range| range.contains(&candidate.address))
            })
        {
            continue;
        }
        entries.push(TraceEntry {
            offset: offset as i64 - target_offset as i64,
            word,
            confidence: if offset == target_offset {
                "data-word-code-pointer"
            } else {
                "nearby-data-word-code-pointer"
            },
        });
    }

    entries
}

fn in_low_kernel_window(addr: u64, spec: &TargetSpec) -> bool {
    (spec.kernel_phys_base..spec.kernel_phys_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr)
}

fn in_high_kernel_window(addr: u64, spec: &TargetSpec) -> bool {
    (spec.kernel_virt_base..spec.kernel_virt_base + RV64_KERNEL_WINDOW_SIZE).contains(&addr)
}

fn is_sv39_canonical(addr: u64) -> bool {
    sv39_sign_extend(addr) == addr
}

fn sv39_sign_extend(addr: u64) -> u64 {
    let sign_bit = 1u64 << (RV64_SV39_BITS - 1);
    let low_mask = (1u64 << RV64_SV39_BITS) - 1;
    let low = addr & low_mask;
    if low & sign_bit == 0 {
        low
    } else {
        low | (!low_mask)
    }
}

fn demangle_symbol(name: &str) -> String {
    rustc_demangle::try_demangle(name)
        .map(|demangled| demangled.to_string())
        .unwrap_or_else(|_| name.to_string())
}

fn print_trap_block(index: usize, trap: &TrapRecord, image: &ElfImage, spec: &TargetSpec) {
    let scause = decode_scause(trap.scause);
    println!("trap #{index}");
    println!();
    println!("scause:");
    println!("  raw: {}", format_hex(scause.raw));
    println!("  kind: {}", scause.kind.label());
    println!("  code: {}", scause.code);
    println!("  name: {}", scause.name);
    println!();

    println!("sepc:");
    print_address_details(trap.sepc, image, spec, None);
    println!();

    println!("stval:");
    print_address_details(trap.stval, image, spec, Some(stval_interpretation(&scause)));
    println!();

    if let Some(frame) = &trap.frame {
        print_trapframe_dump(frame);
        println!();
    }

    println!("candidate trace:");
    print_candidate_trace("sepc", trap.sepc, image, spec);
    print_candidate_trace("stval", trap.stval, image, spec);
}

fn print_trapframe_dump(frame: &TrapFrameDump) {
    println!("trapframe:");
    for (index, value) in frame.x.iter().enumerate() {
        println!(
            "  x{index:02} ({:>4}): {}",
            RV64_REG_NAMES[index],
            format_hex(*value)
        );
    }
    println!("  scause: {}", format_hex(frame.scause));
    println!("  sepc: {}", format_hex(frame.sepc));
    println!("  stval: {}", format_hex(frame.stval));
    println!("  sstatus: {}", format_hex(frame.sstatus));
}

fn print_address_block(label: &str, addr: u64, image: &ElfImage, spec: &TargetSpec) {
    println!("{label}:");
    print_address_details(addr, image, spec, None);
    println!();
    println!("candidate trace:");
    print_candidate_trace(label, addr, image, spec);
}

fn print_address_details(
    addr: u64,
    image: &ElfImage,
    spec: &TargetSpec,
    interpretation: Option<&'static str>,
) {
    let analysis = image.analyze_address(addr, spec);
    println!("  raw: {}", format_hex(analysis.raw));
    if let Some(interpretation) = interpretation {
        println!("  interpretation: {interpretation}");
    }
    println!("  runtime class: {}", analysis.runtime_class.label());
    if let RuntimeClass::DirectMap { phys, region } = &analysis.runtime_class {
        println!("  physical address: {}", format_hex(*phys));
        if let Some(region) = region {
            println!(
                "  region: {region} [{}..{})",
                format_hex(spec.firmware_gap.start),
                format_hex(spec.firmware_gap.end)
            );
        }
    }
    if !analysis.candidates.is_empty() {
        println!("  ELF lookup candidates:");
        for candidate in &analysis.candidates {
            println!("    {}: {}", candidate.label, format_hex(candidate.address));
        }
    }
    if let Some(selected) = &analysis.selected {
        println!("  selected lookup: {}", format_hex(selected.address));
    }
    if let Some(section) = &analysis.section_name {
        println!("  section: {section}");
    }
    if let Some((name, offset)) = &analysis.nearest {
        println!("  symbol: {name} + {:#x}", offset);
    }
    if analysis.frames.is_empty() {
        println!("  source: <unavailable>");
    } else {
        for frame in &analysis.frames {
            let function = frame.function.as_deref().unwrap_or("<unknown>");
            let source = match (&frame.file, frame.line, frame.column) {
                (Some(file), Some(line), Some(column)) => format!("{file}:{line}:{column}"),
                (Some(file), Some(line), None) => format!("{file}:{line}"),
                (Some(file), None, _) => file.clone(),
                _ => "<unavailable>".to_string(),
            };
            println!("  frame: {function} at {source}");
        }
    }
    println!("  confidence: {}", analysis.confidence);
    if let Some(note) = analysis.data_note {
        println!("  note: {note}");
    }
}

fn print_candidate_trace(label: &str, addr: u64, image: &ElfImage, spec: &TargetSpec) {
    let analysis = image.analyze_address(addr, spec);
    println!("  {label}:");
    if !analysis.data_trace.is_empty() {
        println!("    candidate code-pointer table entries:");
        for entry in analysis.data_trace {
            println!(
                "      {:+#x}: {} confidence: {}",
                entry.offset,
                format_hex(entry.word),
                entry.confidence
            );
        }
    } else if analysis.confidence == "direct-code"
        || analysis.confidence == "normalized-code-pointer"
    {
        if let Some((name, offset)) = analysis.nearest {
            println!("    {}: {name} + {:#x}", analysis.confidence, offset);
        } else {
            println!("    {}", analysis.confidence);
        }
    } else if let RuntimeClass::DirectMap { .. } = analysis.runtime_class {
        println!("    unresolved as code pointer");
        println!("    note: direct-map physical address, not an ELF code address");
    } else if let Some(note) = analysis.data_note {
        println!("    unresolved");
        println!("    note: {note}");
    } else {
        println!("    unresolved");
    }
}

fn format_hex(value: u64) -> String {
    format!("0x{value:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serial_trap_variants_and_underscored_hex() {
        let serial = "\
trap: scause=0x0000000000000007 sepc=0xffff_ffff_8021_9096 stval=0xffff_ffc0_8000_0000
txkernel:qemu-riscv64-virt:trap scause=0xf sepc=0x80219096 stval=0x0
";

        let traps = parse_traps(serial);

        assert_eq!(
            traps,
            vec![
                TrapRecord {
                    scause: 7,
                    sepc: 0xffff_ffff_8021_9096,
                    stval: 0xffff_ffc0_8000_0000,
                    frame: None,
                },
                TrapRecord {
                    scause: 15,
                    sepc: 0x8021_9096,
                    stval: 0,
                    frame: None,
                },
            ]
        );
    }

    #[test]
    fn parses_rich_trapframe_dump_without_duplicate_trap() {
        let mut serial = String::from(
            "\
txkernel:qemu-riscv64-virt:trap
scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0000004000001000
trapframe:
",
        );
        for base in (0..32).step_by(4) {
            serial.push_str(&format!(
                "  x{}=0x{:016x} x{}=0x{:016x} x{}=0x{:016x} x{}=0x{:016x}\n",
                base,
                base,
                base + 1,
                base + 1,
                base + 2,
                base + 2,
                base + 3,
                base + 3
            ));
        }
        serial.push_str(
            "  scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0000004000001000 sstatus=0x0000000000000100\n",
        );

        let traps = parse_traps(&serial);

        assert_eq!(traps.len(), 1);
        let trap = &traps[0];
        assert_eq!(trap.scause, 13);
        assert_eq!(trap.sepc, 0xffff_ffff_8020_1234);
        assert_eq!(trap.stval, 0x0000_0040_0000_1000);

        let frame = trap
            .frame
            .as_deref()
            .expect("trapframe dump should be attached");
        assert_eq!(frame.x[0], 0);
        assert_eq!(frame.x[2], 2);
        assert_eq!(frame.x[31], 31);
        assert_eq!(frame.scause, trap.scause);
        assert_eq!(frame.sepc, trap.sepc);
        assert_eq!(frame.stval, trap.stval);
        assert_eq!(frame.sstatus, 0x100);
    }

    #[test]
    fn decodes_scause_kind_code_and_name() {
        assert_eq!(
            decode_scause(7),
            ScauseInfo {
                raw: 7,
                kind: TrapKind::SynchronousException,
                code: 7,
                name: "store/AMO access fault",
            }
        );
        assert_eq!(decode_scause(15).name, "store/AMO page fault");
        assert_eq!(decode_scause(1).name, "instruction access fault");
        assert_eq!(decode_scause(12).name, "instruction page fault");
        assert_eq!(decode_scause(2).name, "illegal instruction");

        let timer = decode_scause(0x8000_0000_0000_0005);
        assert_eq!(timer.kind, TrapKind::Interrupt);
        assert_eq!(timer.code, 5);
        assert_eq!(timer.name, "supervisor timer interrupt");
    }

    #[test]
    fn interprets_stval_by_scause() {
        assert_eq!(
            stval_interpretation(&decode_scause(7)),
            "likely faulting store/AMO access address"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(13)),
            "likely faulting load page address"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(2)),
            "may contain illegal instruction bits"
        );
    }

    #[test]
    fn detects_low_and_high_link_modes_from_symbols() {
        let spec = TargetSpec::rv64_qemu();
        assert_eq!(
            detect_link_mode(
                &[SymbolInfo {
                    name: "__kernel_start".into(),
                    address: 0x8020_0000,
                    size: 0,
                }],
                &spec
            ),
            Some(KernelLinkMode::LowLinkedHighAlias {
                elf_base: 0x8020_0000,
                high_alias_base: 0xffff_ffff_8020_0000,
            })
        );
        assert_eq!(
            detect_link_mode(
                &[SymbolInfo {
                    name: "rust_entry".into(),
                    address: 0xffff_ffff_8020_141a,
                    size: 0,
                }],
                &spec
            ),
            Some(KernelLinkMode::HighLinkedLowLoaded {
                kernel_virt_base: 0xffff_ffff_8020_0000,
                kernel_phys_base: 0x8020_0000,
            })
        );
    }

    #[test]
    fn classifies_direct_map_and_firmware_gap() {
        let spec = TargetSpec::rv64_qemu();
        assert_eq!(
            classify_runtime_address(0xffff_ffff_8021_9096, &spec),
            RuntimeClass::HighKernelAlias
        );
        assert_eq!(
            classify_runtime_address(0xffff_ffc0_8000_0000, &spec),
            RuntimeClass::DirectMap {
                phys: 0x8000_0000,
                region: Some("reserved firmware/loader gap"),
            }
        );
    }

    #[test]
    fn generates_low_high_and_suspicious_candidates() {
        let spec = TargetSpec::rv64_qemu();
        let low = address_candidates(
            0xffff_ffff_8021_9096,
            KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            &spec,
        );
        assert!(low.contains(&AddressCandidate {
            label: "low-linked ELF candidate",
            address: 0x8021_9096,
        }));

        let high = address_candidates(
            0xffff_ffff_8021_9096,
            KernelLinkMode::HighLinkedLowLoaded {
                kernel_virt_base: spec.kernel_virt_base,
                kernel_phys_base: spec.kernel_phys_base,
            },
            &spec,
        );
        assert!(high.contains(&AddressCandidate {
            label: "raw address",
            address: 0xffff_ffff_8021_9096,
        }));
        assert!(!high.iter().any(|candidate| {
            candidate.label == "selected normalization" && candidate.address == 0x8021_9096
        }));

        let suspicious = address_candidates(
            0x1fff_ffff_f004_03a3,
            KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            &spec,
        );
        assert!(suspicious
            .iter()
            .any(|candidate| candidate.label == "low 32 bits"));
        assert!(suspicious
            .iter()
            .any(|candidate| candidate.label == "Sv39 canonical-like form"));
    }

    #[test]
    fn nearest_symbol_reports_symbol_plus_offset() {
        let symbols = vec![
            SymbolInfo {
                name: "first".into(),
                address: 0x1000,
                size: 0x20,
            },
            SymbolInfo {
                name: "target".into(),
                address: 0x2000,
                size: 0x40,
            },
        ];

        assert_eq!(
            nearest_symbol(&symbols, 0x2016),
            Some(("target".into(), 0x16))
        );
    }

    #[test]
    fn scans_absolute_code_pointers_but_not_non_code_words() {
        let spec = TargetSpec::rv64_qemu();
        let mut data = [0u8; 24];
        data[0..8].copy_from_slice(&0x8021_9096u64.to_le_bytes());
        data[8..16].copy_from_slice(&3u64.to_le_bytes());
        data[16..24].copy_from_slice(&0xffff_ffff_8021_90c0u64.to_le_bytes());
        let text_range = 0x8021_0000..0x8022_0000;

        let entries = scan_data_code_pointers(
            0x8023_f120,
            0x8023_f120,
            &data,
            KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            &spec,
            std::slice::from_ref(&text_range),
        );

        assert_eq!(
            entries,
            vec![
                TraceEntry {
                    offset: 0,
                    word: 0x8021_9096,
                    confidence: "data-word-code-pointer",
                },
                TraceEntry {
                    offset: 16,
                    word: 0xffff_ffff_8021_90c0,
                    confidence: "nearby-data-word-code-pointer",
                },
            ]
        );
    }

    #[test]
    fn bss_sections_are_not_scanned() {
        let spec = TargetSpec::rv64_qemu();
        let image = ElfImage {
            layout: KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            loader: None,
            sections: vec![SectionInfo {
                name: ".bss".into(),
                address: 0x8024_0000,
                size: 0x1000,
                kind: SectionKind::UninitializedData,
                data: None,
            }],
            symbols: Vec::new(),
        };

        let (entries, note) = image.trace_data_candidates(0x8024_0010, &spec);

        assert!(entries.is_empty());
        assert_eq!(note, Some("no file contents available for static scan"));
    }
}
