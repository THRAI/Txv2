use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use addr2line::Loader;
use gimli::RunTimeEndian;
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
    let user_image: Option<ElfImage> = config.user_elf
        .as_deref()
        .and_then(|path| ElfImage::load_user(path).ok());

    if !config.json {
        println!("txKernel fault-decode: {}", spec.name);
        println!("ELF: {}", elf_path.display());
        if let Some(id) = &image.build_id {
            println!("build-id: {id}");
        }
        println!("ELF layout: {}", image.layout.label());
        println!();
    }

    match config.input {
        FaultDecodeInput::Address(addr) => {
            if config.json {
                emit_json_address("address", addr, &image, &spec);
            } else {
                print_address_block("address", addr, &image, user_image.as_ref(), &spec);
            }
        }
        FaultDecodeInput::Trap(trap) => {
            if config.brief {
                print_brief_trap(1, &trap, &image, &spec);
            } else if config.json {
                emit_json_trap(1, &trap, &image, &spec);
            } else {
                print_trap_block(1, &trap, &image, user_image.as_ref(), &spec);
            }
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
            if config.json {
                let values: Vec<serde_json::Value> = selected
                    .iter()
                    .enumerate()
                    .map(|(index, trap)| build_json_trap(index + 1, trap, &image, &spec))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&values)
                        .unwrap_or_else(|_| "[]".to_string())
                );
            } else if config.summary {
                print_trap_summary_table(&selected, &image, &spec);
            } else {
                for (index, trap) in selected.iter().enumerate() {
                    if config.brief {
                        print_brief_trap(index + 1, trap, &image, &spec);
                    } else {
                        print_trap_block(index + 1, trap, &image, user_image.as_ref(), &spec);
                        if index + 1 != selected.len() {
                            println!();
                        }
                    }
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
    user_elf: Option<PathBuf>,
    brief: bool,
    json: bool,
    summary: bool,
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
        let user_elf = optional_option_value(args, "--user-elf")
            .map(|path| resolve_path(root, path.into()));
        let all = args.iter().any(|arg| arg == "--all");
        let brief = args.iter().any(|arg| arg == "--brief");
        let json = args.iter().any(|arg| arg == "--json");
        let summary = args.iter().any(|arg| arg == "--summary");

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
                    fp_chain: vec![],
                    panic_msg: None,
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

        Ok(Self { target, elf, input, user_elf, brief, json, summary })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrapRecord {
    scause: u64,
    sepc: u64,
    stval: u64,
    frame: Option<Box<TrapFrameDump>>,
    /// Frame-pointer chain emitted by the kernel: (fp, saved_ra) pairs in
    /// innermost-first order. fp[0].saved_ra == the return address of the
    /// faulting function; fp[1].saved_ra == its caller's, etc.
    fp_chain: Vec<(u64, u64)>,
    /// Panic message from the serial log seen within 30 lines before this trap.
    panic_msg: Option<String>,
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


#[derive(Clone, Debug, Eq, PartialEq)]
struct ScauseInfo {
    raw: u64,
    kind: TrapKind,
    code: u64,
    name: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SstatusInfo {
    raw: u64,
    /// Previous privilege mode (SPP bit 8): true = S-mode, false = U-mode.
    spp: bool,
    /// Supervisor interrupt enable (SIE bit 1).
    sie: bool,
    /// Supervisor previous interrupt enable (SPIE bit 5).
    spie: bool,
    /// Supervisor user memory access (SUM bit 18).
    sum: bool,
    /// Make executable readable (MXR bit 19).
    mxr: bool,
    /// Floating-point unit state (FS bits 14:13): 0=Off 1=Initial 2=Clean 3=Dirty.
    fs: u8,
}

impl SstatusInfo {
    fn spp_label(&self) -> &'static str {
        if self.spp { "S" } else { "U" }
    }

    fn fs_label(&self) -> &'static str {
        match self.fs {
            0 => "Off",
            1 => "Initial",
            2 => "Clean",
            _ => "Dirty",
        }
    }
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
struct FormalParam {
    name: String,
    /// Register value or constant; None if stack-spilled or unevaluable.
    value: Option<u64>,
    /// Which x-register held the value (0–31), if any.
    register: Option<usize>,
    /// True when `value` is a pointer to the actual param (stack spill address).
    is_indirect: bool,
    /// DWARF type name, if resolvable.
    type_name: Option<String>,
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
    bytes: Vec<u8>,
    layout: KernelLinkMode,
    loader: Option<Loader>,
    sections: Vec<SectionInfo>,
    symbols: Vec<SymbolInfo>,
    build_id: Option<String>,
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
        let build_id = extract_build_id(&bytes);

        Ok(Self {
            bytes,
            layout,
            loader,
            sections,
            symbols,
            build_id,
        })
    }

    fn load_user(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let file = object::File::parse(&*bytes)
            .map_err(|err| format!("failed to parse ELF {}: {err}", path.display()))?;

        let mut symbols = Vec::new();
        for symbol in file.symbols() {
            if !symbol.is_definition() || symbol.address() == 0 { continue; }
            if !matches!(symbol.kind(), SymbolKind::Text | SymbolKind::Label | SymbolKind::Data | SymbolKind::Unknown) { continue; }
            let Ok(name) = symbol.name() else { continue; };
            symbols.push(SymbolInfo {
                name: demangle_symbol(name),
                address: symbol.address(),
                size: symbol.size(),
            });
        }
        symbols.sort_by_key(|s| s.address);

        // User ELF: no kernel-specific layout detection needed.
        let layout = KernelLinkMode::LowLinkedHighAlias {
            elf_base: 0,
            high_alias_base: 0,
        };
        let build_id = extract_build_id(&bytes);
        Ok(Self {
            bytes,
            layout,
            loader: Loader::new(path).ok(),
            sections: Vec::new(),
            symbols,
            build_id,
        })
    }

    fn params_at(&self, addr: u64, regs: &[u64; 32]) -> Vec<FormalParam> {
        if self.bytes.is_empty() {
            return Vec::new();
        }
        let Ok(file) = object::File::parse(self.bytes.as_slice()) else {
            return Vec::new();
        };
        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };
        let load = |id: gimli::SectionId| -> core::result::Result<_, gimli::Error> {
            let data = file
                .section_by_name(id.name())
                .and_then(|s| s.data().ok())
                .unwrap_or(&[]);
            Ok(gimli::EndianSlice::new(data, endian))
        };
        let Ok(dwarf) = gimli::Dwarf::load(load) else {
            return Vec::new();
        };
        collect_params(&dwarf, addr, regs)
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

    fn bytes_at(&self, addr: u64) -> Option<[u8; 4]> {
        let section = self.sections.iter().find(|s| {
            s.size >= 4 && s.address <= addr && addr + 4 <= s.address + s.size
        })?;
        let data = section.data.as_deref()?;
        let offset = (addr - section.address) as usize;
        Some(data.get(offset..offset + 4)?.try_into().ok()?)
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

fn extract_type_name<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    entry: &gimli::DebuggingInformationEntry<R>,
) -> Option<String> {
    let type_offset = match entry.attr_value(gimli::DW_AT_type)? {
        gimli::AttributeValue::UnitRef(off) => off,
        _ => return None,
    };
    let type_entry = unit.entry(type_offset).ok()?;
    let name_attr = type_entry.attr_value(gimli::DW_AT_name)?;
    dwarf
        .attr_string(unit, name_attr)
        .ok()
        .and_then(|s| s.to_string_lossy().ok().map(|c| c.into_owned()))
}

fn extract_build_id(elf_bytes: &[u8]) -> Option<String> {
    let file = object::File::parse(elf_bytes).ok()?;
    let section = file
        .section_by_name(".note.gnu.build-id")
        .or_else(|| file.section_by_name(".note.gnu.build_id"))?;
    let data = section.data().ok()?;
    // ELF note header: namesz(4), descsz(4), type(4), then name then desc
    if data.len() < 16 {
        return None;
    }
    let namesz = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    let descsz = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let note_type = u32::from_le_bytes(data[8..12].try_into().ok()?);
    if note_type != 3 {
        return None; // NT_GNU_BUILD_ID = 3
    }
    let name_start = 12;
    let name_end = name_start + namesz;
    let desc_start = (name_end + 3) & !3;
    let desc_end = desc_start + descsz;
    if desc_end > data.len() {
        return None;
    }
    if !data.get(name_start..name_end)?.starts_with(b"GNU") {
        return None;
    }
    let id_bytes = data.get(desc_start..desc_end)?;
    Some(id_bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn collect_params<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    addr: u64,
    regs: &[u64; 32],
) -> Vec<FormalParam> {
    let mut iter = dwarf.units();
    while let Ok(Some(header)) = iter.next() {
        let Ok(unit) = dwarf.unit(header) else { continue };
        let Ok(mut tree) = unit.entries_tree(None) else { continue };
        let Ok(root) = tree.root() else { continue };
        if let Some(params) = find_params_in_node(dwarf, &unit, root, addr, regs) {
            return params;
        }
    }
    Vec::new()
}

fn find_params_in_node<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    node: gimli::EntriesTreeNode<'_, '_, R>,
    addr: u64,
    regs: &[u64; 32],
) -> Option<Vec<FormalParam>> {
    let is_subprogram = node.entry().tag() == gimli::DW_TAG_subprogram;
    let in_range = is_subprogram && die_contains_addr(node.entry(), addr);
    let mut children = node.children();

    if is_subprogram && in_range {
        let mut params = Vec::new();
        while let Ok(Some(child)) = children.next() {
            if child.entry().tag() == gimli::DW_TAG_formal_parameter {
                if let Some(p) = extract_formal_param(dwarf, unit, child.entry(), addr, regs) {
                    params.push(p);
                }
            }
        }
        return Some(params);
    }
    if is_subprogram {
        return None; // wrong subprogram, don't descend
    }
    while let Ok(Some(child)) = children.next() {
        if let Some(params) = find_params_in_node(dwarf, unit, child, addr, regs) {
            return Some(params);
        }
    }
    None
}

fn die_contains_addr<R: gimli::Reader>(
    entry: &gimli::DebuggingInformationEntry<R>,
    addr: u64,
) -> bool {
    let low_pc = match entry.attr_value(gimli::DW_AT_low_pc) {
        Some(gimli::AttributeValue::Addr(pc)) => pc,
        _ => return false,
    };
    let high_pc = match entry.attr_value(gimli::DW_AT_high_pc) {
        Some(gimli::AttributeValue::Addr(pc)) => pc,
        Some(gimli::AttributeValue::Udata(off)) => low_pc.saturating_add(off),
        _ => return low_pc == addr,
    };
    (low_pc..high_pc).contains(&addr)
}

fn extract_formal_param<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    entry: &gimli::DebuggingInformationEntry<R>,
    addr: u64,
    regs: &[u64; 32],
) -> Option<FormalParam> {
    let name = entry
        .attr_value(gimli::DW_AT_name)
        .and_then(|v| dwarf.attr_string(unit, v).ok())
        .and_then(|s| s.to_string_lossy().ok().map(|c| c.into_owned()))
        .unwrap_or_else(|| "_".to_string());

    // Constant — no register involved.
    if let Some(cv) = entry.attr_value(gimli::DW_AT_const_value) {
        let value = match cv {
            gimli::AttributeValue::Data1(v) => Some(v as u64),
            gimli::AttributeValue::Data2(v) => Some(v as u64),
            gimli::AttributeValue::Data4(v) => Some(v as u64),
            gimli::AttributeValue::Data8(v) => Some(v),
            gimli::AttributeValue::Sdata(v) => Some(v as u64),
            gimli::AttributeValue::Udata(v) => Some(v),
            _ => None,
        };
        let type_name = extract_type_name(dwarf, unit, entry);
        return Some(FormalParam { name, value, register: None, is_indirect: false, type_name });
    }

    let type_name = extract_type_name(dwarf, unit, entry);
    let loc_attr = entry.attr_value(gimli::DW_AT_location)?;

    let expr: gimli::Expression<R> = match loc_attr {
        gimli::AttributeValue::Exprloc(e) => e,
        gimli::AttributeValue::LocationListsRef(raw) => {
            // Convert raw offset to RangeListsOffset and walk the list.
            let offset = gimli::LocationListsOffset(raw.0);
            let mut locs = dwarf.locations(unit, offset).ok()?;
            let mut found = None;
            while let Ok(Some(loc_entry)) = locs.next() {
                if (loc_entry.range.begin..loc_entry.range.end).contains(&addr) {
                    found = Some(loc_entry.data);
                    break;
                }
            }
            found?
        }
        _ => return Some(FormalParam { name, value: None, register: None, is_indirect: false, type_name }),
    };

    let (value, register, is_indirect) = eval_location_expr(expr, unit.encoding(), regs);
    Some(FormalParam { name, value, register, is_indirect, type_name })
}

fn eval_location_expr<R: gimli::Reader>(
    expr: gimli::Expression<R>,
    encoding: gimli::Encoding,
    regs: &[u64; 32],
) -> (Option<u64>, Option<usize>, bool) {
    let mut eval = expr.evaluation(encoding);
    let mut state = match eval.evaluate() {
        Ok(s) => s,
        Err(_) => return (None, None, false),
    };
    loop {
        match state {
            gimli::EvaluationResult::Complete => break,
            gimli::EvaluationResult::RequiresRegister { register, .. } => {
                let val = regs.get(register.0 as usize).copied().unwrap_or(0);
                state = match eval.resume_with_register(gimli::Value::Generic(val)) {
                    Ok(s) => s,
                    Err(_) => return (None, None, false),
                };
            }
            _ => return (None, None, false),
        }
    }
    let pieces = eval.result();
    for piece in pieces {
        match piece.location {
            gimli::Location::Register { register } => {
                let idx = register.0 as usize;
                return (regs.get(idx).copied(), Some(idx), false);
            }
            gimli::Location::Address { address } => {
                return (Some(address), None, true);
            }
            gimli::Location::Value { value } => {
                let v: u64 = match value {
                    gimli::Value::Generic(v) => v,
                    gimli::Value::U8(v) => v as u64,
                    gimli::Value::U16(v) => v as u64,
                    gimli::Value::U32(v) => v as u64,
                    gimli::Value::U64(v) => v,
                    gimli::Value::I8(v) => v as u64,
                    gimli::Value::I16(v) => v as u64,
                    gimli::Value::I32(v) => v as u64,
                    gimli::Value::I64(v) => v as u64,
                    _ => return (None, None, false),
                };
                return (Some(v), None, false);
            }
            _ => {}
        }
    }
    (None, None, false)
}

fn parse_traps(serial: &str) -> Vec<TrapRecord> {
    let lines = serial.lines().collect::<Vec<_>>();
    let mut traps = Vec::new();
    let mut index = 0;
    // (message text, line index) — cleared once attached to a trap
    let mut last_panic: Option<(String, usize)> = None;

    while index < lines.len() {
        let line = lines[index];

        if line.contains("panicked at")
            || line.starts_with("PANIC:")
            || line.contains("rust_begin_unwind")
        {
            last_panic = Some((line.to_string(), index));
            index += 1;
            continue;
        }

        let Some(mut trap) = parse_trap_summary(line) else {
            index += 1;
            continue;
        };

        if let Some((msg, panic_index)) = last_panic.take() {
            if index.saturating_sub(panic_index) <= 30 {
                trap.panic_msg = Some(msg);
            }
        }

        let mut next = index + 1;
        while next < lines.len() && lines[next].trim().is_empty() {
            next += 1;
        }

        if next < lines.len() && lines[next].trim() == "trapframe:" {
            let (frame, consumed) = parse_trapframe_block(&lines[next + 1..]);
            trap.frame = frame;
            index = next + 1 + consumed;

            // Look for an optional fp chain: block immediately after the trapframe.
            let mut fp_next = index;
            while fp_next < lines.len() && lines[fp_next].trim().is_empty() {
                fp_next += 1;
            }
            if fp_next < lines.len() && lines[fp_next].trim() == "fp chain:" {
                let (chain, fp_consumed) = parse_fp_chain_block(&lines[fp_next + 1..]);
                trap.fp_chain = chain;
                index = fp_next + 1 + fp_consumed;
            }
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
        fp_chain: vec![],
        panic_msg: None,
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

fn parse_fp_chain_block(lines: &[&str]) -> (Vec<(u64, u64)>, usize) {
    let mut chain = Vec::new();
    let mut consumed = 0;
    for line in lines {
        if line.trim().is_empty() || !(line.starts_with(' ') || line.starts_with('\t')) {
            break;
        }
        consumed += 1;
        if let (Some(fp), Some(ra)) = (
            parse_value_after_key(line, "fp"),
            parse_value_after_key(line, "ra"),
        ) {
            chain.push((fp, ra));
        }
    }
    (chain, consumed)
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
            11 => "machine environment call",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store/AMO page fault",
            _ => "unknown synchronous exception",
        },
        TrapKind::Interrupt => match code {
            0 => "user software interrupt",
            1 => "supervisor software interrupt",
            3 => "machine software interrupt",
            4 => "user timer interrupt",
            5 => "supervisor timer interrupt",
            7 => "machine timer interrupt",
            8 => "user external interrupt",
            9 => "supervisor external interrupt",
            11 => "machine external interrupt",
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

fn decode_sstatus(raw: u64) -> SstatusInfo {
    SstatusInfo {
        raw,
        spp:  raw & (1 << 8)  != 0,
        sie:  raw & (1 << 1)  != 0,
        spie: raw & (1 << 5)  != 0,
        sum:  raw & (1 << 18) != 0,
        mxr:  raw & (1 << 19) != 0,
        fs:   ((raw >> 13) & 3) as u8,
    }
}

fn stval_interpretation(scause: &ScauseInfo) -> &'static str {
    if scause.kind == TrapKind::Interrupt {
        return "not defined for interrupts";
    }
    match scause.code {
        0 => "likely faulting instruction alignment address",
        1 => "likely faulting instruction access address",
        2 => "illegal instruction encoding bits",
        3 => "likely address of breakpoint instruction",
        4 => "likely faulting load alignment address",
        5 => "likely faulting load access address",
        6 => "likely faulting store/AMO alignment address",
        7 => "likely faulting store/AMO access address",
        8 | 9 | 11 => "zero (ecall does not set stval)",
        12 => "likely faulting instruction page address",
        13 => "likely faulting load page address",
        15 => "likely faulting store/AMO page address",
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
        .filter(|symbol| symbol.address <= addr && !symbol.name.starts_with('.'))
        .max_by_key(|symbol| symbol.address)
        .map(|symbol| (symbol.name.clone(), addr - symbol.address))
}

fn null_deref_note(stval: u64) -> Option<&'static str> {
    if stval < 0x1000 {
        Some("likely null pointer dereference")
    } else {
        None
    }
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

fn print_trap_block(index: usize, trap: &TrapRecord, image: &ElfImage, user_image: Option<&ElfImage>, spec: &TargetSpec) {
    let scause = decode_scause(trap.scause);
    let mode_suffix = trap
        .frame
        .as_deref()
        .map(|f| format!("  from {}-mode", decode_sstatus(f.sstatus).spp_label()))
        .unwrap_or_default();

    println!("trap #{index}");
    println!();
    println!("fault: {}  ({}{})", scause.name, format_hex(scause.raw), mode_suffix);
    println!();

    println!("call stack:");
    let stval_info = Some((trap.stval, &scause));
    if let Some(frame) = &trap.frame {
        print_stack_frame(0, trap.sepc, Some(&frame.x), stval_info, image, spec);
        if !trap.fp_chain.is_empty() {
            // fp_chain[0].saved_ra == x1 (ra register); use it as frame #1.
            let text_ranges = image.text_ranges();
            for (fi, &(_fp, saved_ra)) in trap.fp_chain.iter().enumerate() {
                if saved_ra == 0 {
                    break;
                }
                let lookup = saved_ra.saturating_sub(1);
                if !text_ranges.iter().any(|r| r.contains(&lookup)) {
                    println!(
                        "  (fp chain broken at frame {} — ra {:#x} not in kernel text)",
                        fi + 1,
                        saved_ra
                    );
                    break;
                }
                print_stack_frame(fi + 1, lookup, None, None, image, spec);
            }
        } else if frame.x[1] != 0 {
            print_stack_frame(1, frame.x[1].saturating_sub(1), None, None, image, spec);
            println!("  (deeper frames unavailable — kernel not built with frame pointers)");
        }
    } else {
        print_stack_frame(0, trap.sepc, None, stval_info, image, spec);
        println!("  (no trap frame — ra unavailable)");
    }
    println!();

    if let Some(frame) = &trap.frame {
        print_trapframe_dump(frame, image, user_image, spec);
        println!();
    }

    if let Some(msg) = &trap.panic_msg {
        println!("panic: {msg}");
        println!();
    }

    println!("candidate trace:");
    print_candidate_trace("sepc", trap.sepc, image, spec);
    print_candidate_trace("stval", trap.stval, image, spec);
}

fn print_stack_frame(
    index: usize,
    addr: u64,
    regs: Option<&[u64; 32]>,
    fault_info: Option<(u64, &ScauseInfo)>,
    image: &ElfImage,
    spec: &TargetSpec,
) {
    let analysis = image.analyze_address(addr, spec);
    let resolved = analysis
        .selected
        .as_ref()
        .map(|c| c.address)
        .unwrap_or(addr);
    let frames = image.frames_for(resolved);

    if frames.is_empty() {
        match &analysis.nearest {
            Some((name, offset)) => {
                println!("  #{index}  {}  → {name}+{offset:#x}", format_hex(addr))
            }
            None => println!("  #{index}  {}  (unresolved)", format_hex(addr)),
        }
    } else {
        for (fi, frame) in frames.iter().enumerate() {
            let func = frame.function.as_deref().unwrap_or("<unknown>");
            let src = match (&frame.file, frame.line, frame.column) {
                (Some(f), Some(l), Some(c)) => format!("{f}:{l}:{c}"),
                (Some(f), Some(l), None) => format!("{f}:{l}"),
                (Some(f), None, _) => f.clone(),
                _ => "<unknown source>".to_string(),
            };
            if fi == 0 {
                println!("  #{index}  {}  → {func}", format_hex(addr));
                println!("            at {src}");
            } else {
                println!("       (inline) → {func}");
                println!("            at {src}");
            }
        }
    }

    if let Some((stval, scause)) = fault_info {
        if let Some(bytes) = image.bytes_at(resolved) {
            println!("            insn: {}", decode_rv64_insn(bytes));
        }
        if let Some(illegal_note) = decode_illegal_insn_stval(scause, stval) {
            println!("            {illegal_note}");
        }
        let stval_analysis = image.analyze_address(stval, spec);
        let class = stval_analysis.runtime_class.label();
        let interp = stval_interpretation(scause);
        let phys_note = if let RuntimeClass::DirectMap { phys, .. } = &stval_analysis.runtime_class
        {
            format!("  phys={}", format_hex(*phys))
        } else {
            String::new()
        };
        let null_note = null_deref_note(stval)
            .map(|n| format!("  — {n} (offset +{stval:#x})"))
            .unwrap_or_default();
        println!(
            "       fault addr: {}  [{class}]{phys_note}  {interp}{null_note}",
            format_hex(stval)
        );
    }

    if let Some(regs) = regs {
        let params = image.params_at(resolved, regs);
        for param in &params {
            let val_str = match (param.value, param.is_indirect) {
                (Some(v), false) => format_hex(v),
                (Some(v), true) => format!("[→{}]", format_hex(v)),
                (None, _) => "<unavailable>".to_string(),
            };
            let reg_label = param
                .register
                .and_then(|r| RV64_REG_NAMES.get(r).copied())
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            let ann = param
                .value
                .filter(|_| !param.is_indirect)
                .and_then(|v| reg_annotation(v, image, spec))
                .map(|s| format!("  {s}"))
                .unwrap_or_default();
            let type_prefix = param
                .type_name
                .as_deref()
                .map(|t| format!("{t} "))
                .unwrap_or_default();
            println!("            {}{}: {}{}{}", type_prefix, param.name, val_str, reg_label, ann);
        }
        if let Some((stval, _)) = fault_info {
            for (i, &val) in regs.iter().enumerate() {
                if val != 0 && val == stval {
                    println!(
                        "            fault addr matches: {} ({})",
                        format_hex(val),
                        RV64_REG_NAMES[i]
                    );
                    break;
                }
            }
        }
    }
}

fn reg_annotation(value: u64, image: &ElfImage, spec: &TargetSpec) -> Option<String> {
    if value == 0 {
        return None;
    }
    let analysis = image.analyze_address(value, spec);
    match analysis.confidence {
        "direct-code" | "normalized-code-pointer" => {
            if let Some((name, offset)) = &analysis.nearest {
                return Some(if *offset == 0 {
                    format!("→ {name}")
                } else {
                    format!("→ {name}+{offset:#x}")
                });
            }
        }
        _ => {}
    }
    match &analysis.runtime_class {
        RuntimeClass::DirectMap { phys, .. } => {
            return Some(format!("[phys {}]", format_hex(*phys)));
        }
        RuntimeClass::UserAddress => return Some("[user]".to_string()),
        _ => {}
    }
    None
}

fn print_trapframe_dump(frame: &TrapFrameDump, image: &ElfImage, user_image: Option<&ElfImage>, spec: &TargetSpec) {
    println!("trapframe:");
    for (index, value) in frame.x.iter().enumerate() {
        let note = reg_annotation(*value, image, spec)
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        let note = if note.is_empty() {
            user_image
                .and_then(|u| nearest_symbol(&u.symbols, *value))
                .filter(|_| *value < RV64_USER_TOP && *value != 0)
                .map(|(name, off)| {
                    if off == 0 { format!("  → user::{name}") }
                    else { format!("  → user::{name}+{off:#x}") }
                })
                .unwrap_or_default()
        } else {
            note
        };
        println!(
            "  x{index:02} ({:>4}): {}{}",
            RV64_REG_NAMES[index],
            format_hex(*value),
            note
        );
    }
    let scause = decode_scause(frame.scause);
    println!("  scause:  {}  ({})", format_hex(scause.raw), scause.name);
    println!("  sepc:    {}", format_hex(frame.sepc));
    println!("  stval:   {}", format_hex(frame.stval));
    let ss = decode_sstatus(frame.sstatus);
    println!(
        "  sstatus: {}  spp={} spie={} sie={} fs={} sum={} mxr={}",
        format_hex(ss.raw),
        ss.spp_label(),
        ss.spie as u8,
        ss.sie as u8,
        ss.fs_label(),
        ss.sum as u8,
        ss.mxr as u8,
    );
}

fn print_address_block(label: &str, addr: u64, image: &ElfImage, user_image: Option<&ElfImage>, spec: &TargetSpec) {
    println!("{label}:");
    print_address_details(addr, image, spec, None);
    if let Some(u) = user_image {
        if addr < RV64_USER_TOP {
            if let Some((name, off)) = nearest_symbol(&u.symbols, addr) {
                println!("  user symbol: {name} + {off:#x}");
            }
        }
    }
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
        if let Some(note) = null_deref_note(addr) {
            println!("  interpretation: {interpretation} — {note} (offset +{addr:#x})");
        } else {
            println!("  interpretation: {interpretation}");
        }
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

fn decode_rv64_insn(b: [u8; 4]) -> String {
    let w = u32::from_le_bytes(b);
    if w & 3 != 3 {
        let op = (w & 3) as u8;
        let f3 = ((w >> 13) & 7) as u8;
        // 3-bit compressed register fields: rd'/rs1'/rs2' → x8..x15
        let rdc = (((w >> 2) & 7) + 8) as usize;
        let rs1c = (((w >> 7) & 7) + 8) as usize;
        let rs2c = (((w >> 2) & 7) + 8) as usize;
        // Full 5-bit rd/rs2 for stack-relative instructions
        let rd5 = ((w >> 7) & 0x1f) as usize;
        let rs2_5 = ((w >> 2) & 0x1f) as usize;
        let bit12 = (w >> 12) & 1;
        return match (op, f3) {
            // Quadrant 0
            (0, 0) => {
                // C.ADDI4SPN
                let nzuimm = ((w >> 6) & 1) << 2
                    | ((w >> 5) & 1) << 3
                    | ((w >> 11) & 0x3) << 4
                    | ((w >> 7) & 0xf) << 6;
                format!("c.addi4spn {}, sp, {nzuimm}", RV64_REG_NAMES[rdc])
            }
            (0, 1) => "c.fld".to_string(),
            (0, 2) => {
                let imm = ((w >> 10) & 7) << 3 | ((w >> 6) & 1) << 7 | ((w >> 5) & 1) << 2;
                format!("c.lw {}, {imm}({})", RV64_REG_NAMES[rdc], RV64_REG_NAMES[rs1c])
            }
            (0, 3) => {
                let imm = ((w >> 10) & 7) << 3 | ((w >> 5) & 3) << 6;
                format!("c.ld {}, {imm}({})", RV64_REG_NAMES[rdc], RV64_REG_NAMES[rs1c])
            }
            (0, 6) => {
                let imm = ((w >> 10) & 7) << 3 | ((w >> 6) & 1) << 7 | ((w >> 5) & 1) << 2;
                format!("c.sw {}, {imm}({})", RV64_REG_NAMES[rs2c], RV64_REG_NAMES[rs1c])
            }
            (0, 7) => {
                let imm = ((w >> 10) & 7) << 3 | ((w >> 5) & 3) << 6;
                format!("c.sd {}, {imm}({})", RV64_REG_NAMES[rs2c], RV64_REG_NAMES[rs1c])
            }
            // Quadrant 1
            (1, 0) => {
                if rd5 == 0 {
                    "c.nop".to_string()
                } else {
                    let imm_raw = ((w >> 12) & 1) << 5 | ((w >> 2) & 0x1f);
                    let imm = ((imm_raw as i32) << 26) >> 26;
                    format!("c.addi {}, {imm}", RV64_REG_NAMES[rd5])
                }
            }
            (1, 1) => {
                let imm_raw = ((w >> 12) & 1) << 5 | ((w >> 2) & 0x1f);
                let imm = ((imm_raw as i32) << 26) >> 26;
                format!("c.addiw {}, {imm}", RV64_REG_NAMES[rd5])
            }
            (1, 2) => {
                let imm_raw = ((w >> 12) & 1) << 5 | ((w >> 2) & 0x1f);
                let imm = ((imm_raw as i32) << 26) >> 26;
                format!("c.li {}, {imm}", RV64_REG_NAMES[rd5])
            }
            (1, 3) => {
                if rd5 == 2 {
                    // C.ADDI16SP
                    let nzimm = ((w >> 12) & 1) << 9
                        | ((w >> 6) & 1) << 4
                        | ((w >> 5) & 1) << 6
                        | ((w >> 3) & 3) << 7
                        | ((w >> 2) & 1) << 5;
                    let nzimm = ((nzimm as i32) << 22) >> 22;
                    format!("c.addi16sp {nzimm}")
                } else {
                    let imm_raw = ((w >> 12) & 1) << 17 | ((w >> 2) & 0x1f) << 12;
                    let imm = ((imm_raw as i32) << 14) >> 14;
                    format!("c.lui {}, {imm:#x}", RV64_REG_NAMES[rd5])
                }
            }
            (1, 4) => {
                let funct2 = (w >> 10) & 3;
                let rs1c_arith = (((w >> 7) & 7) + 8) as usize;
                let rs2c_arith = (((w >> 2) & 7) + 8) as usize;
                match funct2 {
                    0 => {
                        if bit12 == 0 {
                            format!("c.srli {}", RV64_REG_NAMES[rs1c_arith])
                        } else {
                            format!("c.srai {}", RV64_REG_NAMES[rs1c_arith])
                        }
                    }
                    1 => {
                        let imm_raw = (bit12 << 5) | ((w >> 2) & 0x1f);
                        let imm = ((imm_raw as i32) << 26) >> 26;
                        format!("c.andi {}, {imm}", RV64_REG_NAMES[rs1c_arith])
                    }
                    3 => {
                        let sub_op = (w >> 5) & 3;
                        if bit12 == 0 {
                            let nm = match sub_op { 0=>"c.sub", 1=>"c.xor", 2=>"c.or", _=>"c.and" };
                            format!("{nm} {}, {}", RV64_REG_NAMES[rs1c_arith], RV64_REG_NAMES[rs2c_arith])
                        } else {
                            let nm = match sub_op { 0=>"c.subw", 1=>"c.addw", _=>"c.arith?" };
                            format!("{nm} {}, {}", RV64_REG_NAMES[rs1c_arith], RV64_REG_NAMES[rs2c_arith])
                        }
                    }
                    _ => format!("compressed (op={op} f3={f3} bits={w:#018b})"),
                }
            }
            (1, 5) => "c.j".to_string(),
            (1, 6) => format!("c.beqz {}", RV64_REG_NAMES[rs1c]),
            (1, 7) => format!("c.bnez {}", RV64_REG_NAMES[rs1c]),
            // Quadrant 2
            (2, 0) => {
                let shamt = (bit12 << 5) | ((w >> 2) & 0x1f);
                format!("c.slli {}, {shamt}", RV64_REG_NAMES[rd5])
            }
            (2, 1) => "c.fldsp".to_string(),
            (2, 2) => {
                let imm = ((w >> 4) & 7) << 2 | (bit12 << 5) | ((w >> 2) & 3) << 6;
                format!("c.lwsp {}, {imm}(sp)", RV64_REG_NAMES[rd5])
            }
            (2, 3) => {
                let imm = ((w >> 5) & 3) << 3 | (bit12 << 5) | ((w >> 2) & 7) << 6;
                format!("c.ldsp {}, {imm}(sp)", RV64_REG_NAMES[rd5])
            }
            (2, 4) => {
                if bit12 == 0 {
                    if rs2_5 == 0 && rd5 != 0 {
                        format!("c.jr {}", RV64_REG_NAMES[rd5])
                    } else if rs2_5 != 0 && rd5 != 0 {
                        format!("c.mv {}, {}", RV64_REG_NAMES[rd5], RV64_REG_NAMES[rs2_5])
                    } else {
                        format!("compressed (op={op} f3={f3} bits={w:#018b})")
                    }
                } else {
                    if rs2_5 == 0 && rd5 == 0 {
                        "c.ebreak".to_string()
                    } else if rs2_5 == 0 && rd5 != 0 {
                        format!("c.jalr {}", RV64_REG_NAMES[rd5])
                    } else {
                        format!("c.add {}, {}", RV64_REG_NAMES[rd5], RV64_REG_NAMES[rs2_5])
                    }
                }
            }
            (2, 6) => {
                let imm = ((w >> 9) & 7) << 2 | ((w >> 7) & 3) << 6;
                format!("c.swsp {}, {imm}(sp)", RV64_REG_NAMES[rs2_5])
            }
            (2, 7) => {
                let imm = ((w >> 10) & 7) << 3 | ((w >> 7) & 7) << 6;
                format!("c.sdsp {}, {imm}(sp)", RV64_REG_NAMES[rs2_5])
            }
            _ => format!("compressed (op={op} f3={f3} bits={w:#018b})"),
        };
    }
    let opcode = (w >> 2) & 0x1f;
    let f3 = (w >> 12) & 7;
    let rd = ((w >> 7) & 0x1f) as usize;
    let rs1 = ((w >> 15) & 0x1f) as usize;
    let rs2 = ((w >> 20) & 0x1f) as usize;
    match opcode {
        0b00000 => {
            let imm = (w as i32) >> 20;
            let sz = match f3 { 0=>"lb",1=>"lh",2=>"lw",3=>"ld",4=>"lbu",5=>"lhu",6=>"lwu",_=>"l?" };
            format!("{sz} {}, {imm}({})", RV64_REG_NAMES[rd], RV64_REG_NAMES[rs1])
        }
        0b01000 => {
            let imm_lo = (w >> 7) & 0x1f;
            let imm_hi = (w >> 25) & 0x7f;
            let imm = (((imm_hi << 5) | imm_lo) as i32) << 20 >> 20;
            let sz = match f3 { 0=>"sb",1=>"sh",2=>"sw",3=>"sd",_=>"s?" };
            format!("{sz} {}, {imm}({})", RV64_REG_NAMES[rs2], RV64_REG_NAMES[rs1])
        }
        0b11000 => {
            let nm = match f3 { 0=>"beq",1=>"bne",4=>"blt",5=>"bge",6=>"bltu",7=>"bgeu",_=>"b?" };
            format!("{nm} {}, {}", RV64_REG_NAMES[rs1], RV64_REG_NAMES[rs2])
        }
        0b11001 => {
            let imm = (w as i32) >> 20;
            format!("jalr {}, {imm}({})", RV64_REG_NAMES[rd], RV64_REG_NAMES[rs1])
        }
        0b11011 => format!("jal {}", RV64_REG_NAMES[rd]),
        0b11100 => match w >> 20 {
            0 => "ecall".to_string(),
            1 => "ebreak".to_string(),
            v => format!("system ({v:#x})"),
        },
        0b00100 => format!("op-imm {}, {}", RV64_REG_NAMES[rd], RV64_REG_NAMES[rs1]),
        0b01100 => format!("op {}, {}, {}", RV64_REG_NAMES[rd], RV64_REG_NAMES[rs1], RV64_REG_NAMES[rs2]),
        0b01101 => format!("lui {}", RV64_REG_NAMES[rd]),
        0b00101 => format!("auipc {}", RV64_REG_NAMES[rd]),
        0b00011 => "fence".to_string(),
        _ => format!("opcode={opcode:#07b}"),
    }
}

fn decode_illegal_insn_stval(scause: &ScauseInfo, stval: u64) -> Option<String> {
    if scause.code != 2 || stval == 0 {
        return None;
    }
    let b = (stval as u32).to_le_bytes();
    let decoded = decode_rv64_insn(b);
    Some(format!("illegal insn: {decoded}"))
}

fn print_trap_summary_table(traps: &[TrapRecord], image: &ElfImage, spec: &TargetSpec) {
    println!(
        "  {:>3}  {:<32}  {:<30}  {:<18}  {}",
        "#", "cause", "sepc→symbol", "stval", "flags"
    );
    println!(
        "  {:>3}  {:<32}  {:<30}  {:<18}  {}",
        "─", "─────", "───────────", "─────", "─────"
    );

    for (i, trap) in traps.iter().enumerate() {
        let scause = decode_scause(trap.scause);
        let cause_str = scause.name;

        let sepc_analysis = image.analyze_address(trap.sepc, spec);
        let selected_addr = sepc_analysis
            .selected
            .as_ref()
            .map(|c| c.address)
            .unwrap_or(trap.sepc);
        let sym_str = match nearest_symbol(&image.symbols, selected_addr) {
            Some((name, 0)) => name,
            Some((name, off)) => format!("{name}+{off:#x}"),
            None => format_hex(trap.sepc),
        };
        // truncate to 30 chars
        let sym_str = if sym_str.len() > 30 {
            sym_str[..30].to_string()
        } else {
            sym_str
        };

        let stval_str = format_hex(trap.stval);

        let mut flags = Vec::new();
        if null_deref_note(trap.stval).is_some() {
            flags.push("null");
        }
        if trap.panic_msg.is_some() {
            flags.push("panic");
        }
        let flags_str = flags.join(" ");

        println!(
            "  {:>3}  {:<32}  {:<30}  {:<18}  {}",
            i + 1,
            cause_str,
            sym_str,
            stval_str,
            flags_str,
        );
    }

    println!();

    // Build histogram sorted by count descending
    let mut counts: Vec<(u64, &'static str, u64)> = Vec::new(); // (scause_raw, name, count)
    for trap in traps {
        let scause = decode_scause(trap.scause);
        if let Some(entry) = counts.iter_mut().find(|e| e.0 == trap.scause) {
            entry.2 += 1;
        } else {
            counts.push((trap.scause, scause.name, 1));
        }
    }
    counts.sort_by(|a, b| b.2.cmp(&a.2));

    println!("scause histogram:");
    for (_, name, count) in &counts {
        println!("  {count}×  {name}");
    }
}

fn format_hex(value: u64) -> String {
    format!("0x{value:016x}")
}

fn print_brief_trap(index: usize, trap: &TrapRecord, image: &ElfImage, spec: &TargetSpec) {
    let scause = decode_scause(trap.scause);
    let sepc_analysis = image.analyze_address(trap.sepc, spec);
    let resolved = sepc_analysis.selected.as_ref().map(|c| c.address).unwrap_or(trap.sepc);
    let sym = nearest_symbol(&image.symbols, resolved)
        .map(|(name, off)| {
            if off == 0 { name } else { format!("{name}+{off:#x}") }
        })
        .unwrap_or_else(|| format_hex(trap.sepc));

    let stval_class = classify_runtime_address(trap.stval, spec);
    let stval_note = match &stval_class {
        RuntimeClass::UserAddress if trap.stval < 0x1000 => format!("null+{:#x}", trap.stval),
        RuntimeClass::UserAddress => format!("user:{:#x}", trap.stval),
        RuntimeClass::DirectMap { phys, .. } => format!("phys:{}", format_hex(*phys)),
        RuntimeClass::HighKernelAlias => format!("kernel:{}", format_hex(trap.stval)),
        _ => format_hex(trap.stval),
    };

    let mode = trap
        .frame
        .as_deref()
        .map(|f| decode_sstatus(f.sstatus).spp_label())
        .unwrap_or("?");

    println!(
        "trap #{index}: {}  {}  @ {}  (from {mode}-mode)",
        scause.name, stval_note, sym
    );
}

fn build_json_trap(
    index: usize,
    trap: &TrapRecord,
    image: &ElfImage,
    spec: &TargetSpec,
) -> serde_json::Value {
    use serde_json::{json, Value};

    let scause = decode_scause(trap.scause);
    let sepc_analysis = image.analyze_address(trap.sepc, spec);
    let resolved = sepc_analysis.selected.as_ref().map(|c| c.address).unwrap_or(trap.sepc);

    let frames: Vec<Value> = {
        let dwarf_frames = image.frames_for(resolved);
        if dwarf_frames.is_empty() {
            let sym = nearest_symbol(&image.symbols, resolved);
            vec![json!({
                "addr": format_hex(trap.sepc),
                "func": sym.as_ref().map(|(n, _)| n.as_str()).unwrap_or("<unknown>"),
                "offset": sym.as_ref().map(|(_, o)| *o),
            })]
        } else {
            dwarf_frames
                .iter()
                .enumerate()
                .map(|(fi, f)| {
                    json!({
                        "addr": if fi == 0 { Value::String(format_hex(trap.sepc)) } else { Value::Null },
                        "func": f.function.as_deref().unwrap_or("<unknown>"),
                        "file": f.file,
                        "line": f.line,
                    })
                })
                .collect()
        }
    };

    let stval_class = classify_runtime_address(trap.stval, spec);
    let null_deref = trap.stval < 0x1000;

    let trapframe_json: Value = if let Some(frame) = &trap.frame {
        let ss = decode_sstatus(frame.sstatus);
        json!({
            "x": frame.x.iter().map(|v| format_hex(*v)).collect::<Vec<_>>(),
            "sstatus": {
                "raw": format_hex(frame.sstatus),
                "spp": ss.spp_label(),
                "spie": ss.spie,
                "sie": ss.sie,
                "fs": ss.fs_label(),
                "sum": ss.sum,
                "mxr": ss.mxr,
            }
        })
    } else {
        Value::Null
    };

    let from_mode: Value = trap
        .frame
        .as_deref()
        .map(|f| Value::String(decode_sstatus(f.sstatus).spp_label().to_string()))
        .unwrap_or(Value::Null);

    json!({
        "trap": index,
        "scause": {
            "raw": format_hex(trap.scause),
            "name": scause.name,
            "kind": match scause.kind {
                TrapKind::SynchronousException => "exception",
                TrapKind::Interrupt => "interrupt",
            },
            "code": scause.code,
        },
        "sepc": format_hex(trap.sepc),
        "stval": format_hex(trap.stval),
        "stval_class": stval_class.label(),
        "null_deref": null_deref,
        "from_mode": from_mode,
        "call_stack": frames,
        "trapframe": trapframe_json,
    })
}

fn emit_json_trap(index: usize, trap: &TrapRecord, image: &ElfImage, spec: &TargetSpec) {
    let obj = build_json_trap(index, trap, image, spec);
    println!(
        "{}",
        serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string())
    );
}

fn emit_json_address(label: &str, addr: u64, image: &ElfImage, spec: &TargetSpec) {
    use serde_json::json;
    let analysis = image.analyze_address(addr, spec);
    let obj = json!({
        "label": label,
        "raw": format_hex(addr),
        "runtime_class": analysis.runtime_class.label(),
        "confidence": analysis.confidence,
        "nearest": analysis.nearest.as_ref().map(|(n, o)| json!({"name": n, "offset": o})),
        "frames": analysis.frames.iter().map(|f| json!({
            "func": f.function,
            "file": f.file,
            "line": f.line,
        })).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string())
    );
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
                    fp_chain: vec![],
                    panic_msg: None,
                },
                TrapRecord {
                    scause: 15,
                    sepc: 0x8021_9096,
                    stval: 0,
                    frame: None,
                    fp_chain: vec![],
                    panic_msg: None,
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
    fn parses_fp_chain_after_trapframe() {
        let mut serial = String::from(
            "trap: scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0\ntrapframe:\n",
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
            "  scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0 sstatus=0x0000000000000100\n",
        );
        serial.push_str("fp chain:\n");
        serial.push_str("  fp=0xffffffff80218f80 ra=0xffffffff80209000\n");
        serial.push_str("  fp=0xffffffff80218fa0 ra=0xffffffff80205500\n");

        let traps = parse_traps(&serial);
        assert_eq!(traps.len(), 1);
        let trap = &traps[0];
        assert_eq!(
            trap.fp_chain,
            vec![
                (0xffffffff80218f80, 0xffffffff80209000),
                (0xffffffff80218fa0, 0xffffffff80205500),
            ]
        );
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
            "illegal instruction encoding bits"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(0)),
            "likely faulting instruction alignment address"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(3)),
            "likely address of breakpoint instruction"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(4)),
            "likely faulting load alignment address"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(6)),
            "likely faulting store/AMO alignment address"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(8)),
            "zero (ecall does not set stval)"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(9)),
            "zero (ecall does not set stval)"
        );
        assert_eq!(
            stval_interpretation(&decode_scause(0x8000_0000_0000_0005)),
            "not defined for interrupts"
        );
    }

    #[test]
    fn decodes_scause_complete_interrupt_codes() {
        let cases = [
            (0u64, "user software interrupt"),
            (1, "supervisor software interrupt"),
            (3, "machine software interrupt"),
            (4, "user timer interrupt"),
            (5, "supervisor timer interrupt"),
            (7, "machine timer interrupt"),
            (8, "user external interrupt"),
            (9, "supervisor external interrupt"),
            (11, "machine external interrupt"),
        ];
        for (code, expected) in cases {
            let raw = (1u64 << 63) | code;
            assert_eq!(decode_scause(raw).name, expected, "code {code}");
        }
    }

    #[test]
    fn decodes_sstatus_fields() {
        // SPP=1 (bit 8), SPIE=1 (bit 5), FS=Clean (bits 14:13 = 0b10)
        let raw = (1 << 8) | (1 << 5) | (2 << 13);
        let ss = decode_sstatus(raw);
        assert!(ss.spp);
        assert!(ss.spie);
        assert!(!ss.sie);
        assert_eq!(ss.fs, 2);
        assert_eq!(ss.spp_label(), "S");
        assert_eq!(ss.fs_label(), "Clean");

        // SPP=0 (U-mode), SIE=1 (bit 1), SUM=1 (bit 18), MXR=1 (bit 19), FS=Dirty
        let raw2 = (1 << 1) | (3 << 13) | (1 << 18) | (1 << 19);
        let ss2 = decode_sstatus(raw2);
        assert!(!ss2.spp);
        assert!(ss2.sie);
        assert!(!ss2.spie);
        assert!(ss2.sum);
        assert!(ss2.mxr);
        assert_eq!(ss2.fs_label(), "Dirty");
        assert_eq!(ss2.spp_label(), "U");
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
    fn reg_annotation_resolves_code_pointers_and_phys_addresses() {
        let spec = TargetSpec::rv64_qemu();
        let image = ElfImage {
            bytes: Vec::new(),
            layout: KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            loader: None,
            sections: vec![SectionInfo {
                name: ".text".into(),
                address: 0x8020_0000,
                size: 0x0010_0000,
                kind: SectionKind::Text,
                data: None,
            }],
            symbols: vec![SymbolInfo {
                name: "handle_fault".into(),
                address: 0x8020_1000,
                size: 0x80,
            }],
            build_id: None,
        };

        // Low-linked code pointer: resolves to symbol+offset
        let ann = reg_annotation(0x8020_1010, &image, &spec);
        assert_eq!(ann, Some("→ handle_fault+0x10".to_string()));

        // High-alias of the same address: normalised to ELF candidate, same symbol
        let ann_high = reg_annotation(0xffff_ffff_8020_1000, &image, &spec);
        assert_eq!(ann_high, Some("→ handle_fault".to_string()));

        // Direct-map physical address: shows phys
        let ann_dm = reg_annotation(0xffff_ffc0_8000_1000, &image, &spec);
        assert!(
            ann_dm.as_deref().unwrap_or("").starts_with("[phys"),
            "expected phys annotation, got {ann_dm:?}"
        );

        // User address
        let ann_user = reg_annotation(0x0000_0000_1234_5678, &image, &spec);
        assert_eq!(ann_user, Some("[user]".to_string()));

        // Zero: no annotation
        assert_eq!(reg_annotation(0, &image, &spec), None);
    }

    #[test]
    fn bss_sections_are_not_scanned() {
        let spec = TargetSpec::rv64_qemu();
        let image = ElfImage {
            bytes: Vec::new(),
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
            build_id: None,
        };

        let (entries, note) = image.trace_data_candidates(0x8024_0010, &spec);

        assert!(entries.is_empty());
        assert_eq!(note, Some("no file contents available for static scan"));
    }

    #[test]
    fn nearest_symbol_skips_local_dot_labels() {
        let symbols = vec![
            SymbolInfo { name: ".L0".into(), address: 0x2010, size: 0 },
            SymbolInfo { name: "real_fn".into(), address: 0x2000, size: 0x40 },
        ];
        assert_eq!(nearest_symbol(&symbols, 0x2016), Some(("real_fn".into(), 0x16)));
    }

    #[test]
    fn null_deref_stval_annotation() {
        assert_eq!(null_deref_note(0x48), Some("likely null pointer dereference"));
        assert_eq!(null_deref_note(0x0), Some("likely null pointer dereference"));
        assert_eq!(null_deref_note(0xfff), Some("likely null pointer dereference"));
        assert_eq!(null_deref_note(0x1000), None);
        assert_eq!(null_deref_note(0x8000_0000), None);
    }

    #[test]
    fn parses_panic_message_before_trap() {
        let serial_close = "\
panicked at 'index out of bounds: len=3 idx=5', kernel/src/foo.rs:42:8\n\
scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0\n";
        let traps = parse_traps(serial_close);
        assert_eq!(traps.len(), 1);
        assert!(traps[0].panic_msg.as_deref().unwrap().contains("index out of bounds"));

        let mut serial_far = String::from(
            "panicked at 'something', kernel/src/bar.rs:10:1\n",
        );
        for i in 0..31 { serial_far.push_str(&format!("log line {i}\n")); }
        serial_far.push_str("scause=0x000000000000000d sepc=0xffffffff80201234 stval=0x0\n");
        let traps_far = parse_traps(&serial_far);
        assert_eq!(traps_far.len(), 1);
        assert_eq!(traps_far[0].panic_msg, None);
    }

    #[test]
    fn decode_rv64_insn_load_store() {
        // ld a0, -8(s0): opcode=LOAD f3=3 rd=a0 rs1=s0 imm=-8
        let ld = decode_rv64_insn(0xFF843503u32.to_le_bytes());
        assert!(ld.contains("ld"), "got '{ld}'");
        assert!(ld.contains("a0"), "got '{ld}'");
        assert!(ld.contains("s0"), "got '{ld}'");

        // sd a0, -16(s0): opcode=STORE f3=3 rs2=a0 rs1=s0 imm=-16
        let sd = decode_rv64_insn(0xFEA43823u32.to_le_bytes());
        assert!(sd.contains("sd"), "got '{sd}'");
        assert!(sd.contains("a0"), "got '{sd}'");
        assert!(sd.contains("s0"), "got '{sd}'");
    }

    #[test]
    fn decode_rv64_insn_compressed() {
        // C.LD: op=0b00 f3=0b011 (low 2 bits=0b00, bits[15:13]=0b011)
        let cld_word: u32 = 0b011_00000_000_000_00;
        let cld = decode_rv64_insn(cld_word.to_le_bytes());
        assert!(cld.contains("c.ld"), "got '{cld}'");
    }

    #[test]
    fn user_elf_annotation_on_user_address() {
        let user_image = ElfImage {
            bytes: Vec::new(),
            layout: KernelLinkMode::LowLinkedHighAlias {
                elf_base: 0,
                high_alias_base: 0,
            },
            loader: None,
            sections: Vec::new(),
            symbols: vec![SymbolInfo {
                name: "user_fn".into(),
                address: 0x1000,
                size: 0x100,
            }],
            build_id: None,
        };

        // Address inside the symbol: should return name + offset
        assert_eq!(
            nearest_symbol(&user_image.symbols, 0x1010),
            Some(("user_fn".into(), 0x10))
        );

        // Address before the symbol: no match (nearest_symbol only looks at addr >= symbol.address)
        assert_eq!(nearest_symbol(&user_image.symbols, 0x0fff), None);
    }

    #[test]
    fn extract_build_id_returns_none_on_empty_and_invalid() {
        assert_eq!(extract_build_id(b""), None);
        assert_eq!(extract_build_id(b"\x00\x00\x00\x00"), None);
    }

    #[test]
    fn formal_param_type_name_defaults_none() {
        let param = FormalParam {
            name: "x".to_string(),
            value: None,
            register: None,
            is_indirect: false,
            type_name: None,
        };
        assert!(param.type_name.is_none());
    }

    #[test]
    fn brief_format_includes_scause_and_null_deref_stval() {
        let spec = TargetSpec::rv64_qemu();
        // null-deref stval should be UserAddress class
        let class = classify_runtime_address(0x48, &spec);
        assert!(matches!(class, RuntimeClass::UserAddress));
        // stval in null range
        assert!(0x48u64 < 0x1000);
        // scause 13 == load page fault
        assert_eq!(decode_scause(13).name, "load page fault");
    }

    #[test]
    fn json_output_is_valid_json() {
        let spec = TargetSpec::rv64_qemu();
        let image = ElfImage {
            bytes: Vec::new(),
            layout: KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            loader: None,
            sections: Vec::new(),
            symbols: Vec::new(),
            build_id: None,
        };
        let trap = TrapRecord {
            scause: 15,
            sepc: 0x8021_5000,
            stval: 0x0,
            frame: None,
            fp_chain: vec![],
            panic_msg: None,
        };
        let val = build_json_trap(1, &trap, &image, &spec);
        let serialized = serde_json::to_string_pretty(&val).expect("serialization must succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("output must be valid JSON");
        assert_eq!(parsed["trap"], 1);
        assert_eq!(parsed["scause"]["name"], "store/AMO page fault");
        assert_eq!(parsed["null_deref"], true);
    }

    #[test]
    fn decode_rv64_insn_compressed_c_jalr() {
        // C.JALR: op=2, f3=100, bit12=1, rd=ra(1), rs2=0
        // word 0x9082 = 0b1001000010000010
        let result = decode_rv64_insn(0x9082u32.to_le_bytes());
        assert!(result.contains("c.jalr"), "got '{result}'");
    }

    #[test]
    fn decode_rv64_insn_compressed_c_addi() {
        // C.ADDI: op=1, f3=0, rd=a0(10), nz_imm=1
        // 0b000_01010_00001_01 = op=1, rd=10, bit12=0, bits[6:2]=1
        let word = 0b000_01010_00001_01u32;
        let result = decode_rv64_insn(word.to_le_bytes());
        assert!(result.contains("c.addi"), "got '{result}'");
        assert!(result.contains("a0"), "got '{result}'");
    }

    #[test]
    fn decode_rv64_insn_compressed_c_beqz() {
        // C.BEQZ: op=1, f3=6, rs1'=0 (=x8), offset=0
        // 0b110_000_000_00000_01
        let word = 0b110_000_000_00000_01u32;
        let result = decode_rv64_insn(word.to_le_bytes());
        assert!(result.contains("c.beqz"), "got '{result}'");
    }

    #[test]
    fn decode_illegal_insn_stval_for_illegal_insn_cause() {
        // scause=2 = illegal instruction; stval=0xFF843503 = ld a0,-8(s0)
        let result = decode_illegal_insn_stval(&decode_scause(2), 0xFF843503);
        assert!(result.is_some(), "expected Some");
        let s = result.unwrap();
        assert!(s.contains("ld"), "got '{s}'");
    }

    #[test]
    fn decode_illegal_insn_stval_none_for_non_illegal() {
        // scause=13 = load page fault; should return None
        let result = decode_illegal_insn_stval(&decode_scause(13), 0xFF843503);
        assert!(result.is_none(), "expected None, got {result:?}");
    }

    #[test]
    fn summary_table_histogram_counts_correctly() {
        let spec = TargetSpec::rv64_qemu();
        let image = ElfImage {
            bytes: Vec::new(),
            layout: KernelLinkMode::LowLinkedHighAlias {
                elf_base: spec.kernel_phys_base,
                high_alias_base: spec.kernel_virt_base,
            },
            loader: None,
            sections: Vec::new(),
            symbols: Vec::new(),
            build_id: None,
        };

        let traps = vec![
            TrapRecord {
                scause: 13,
                sepc: 0x8020_1000,
                stval: 0x48,
                frame: None,
                fp_chain: vec![],
                panic_msg: None,
            },
            TrapRecord {
                scause: 13,
                sepc: 0x8020_2000,
                stval: 0x8020_3000,
                frame: None,
                fp_chain: vec![],
                panic_msg: Some("panic!".to_string()),
            },
            TrapRecord {
                scause: 15,
                sepc: 0x8020_4000,
                stval: 0x8020_5000,
                frame: None,
                fp_chain: vec![],
                panic_msg: None,
            },
        ];

        // Should not panic
        print_trap_summary_table(&traps, &image, &spec);

        // Verify histogram logic inline
        let mut counts: Vec<(u64, u64)> = Vec::new(); // (scause_raw, count)
        for trap in &traps {
            if let Some(entry) = counts.iter_mut().find(|e| e.0 == trap.scause) {
                entry.1 += 1;
            } else {
                counts.push((trap.scause, 1));
            }
        }
        let count_13 = counts.iter().find(|e| e.0 == 13).map(|e| e.1).unwrap_or(0);
        let count_15 = counts.iter().find(|e| e.0 == 15).map(|e| e.1).unwrap_or(0);
        assert_eq!(count_13, 2, "expected 2 entries for scause=13 (load page fault)");
        assert_eq!(count_15, 1, "expected 1 entry for scause=15 (store/AMO page fault)");
    }

    #[test]
    fn summary_flag_parsed_from_args() {
        let root = std::path::Path::new(".");
        let args: Vec<String> = ["--target", "rv64-qemu", "--summary", "--addr", "0x80200000"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let config = FaultDecodeConfig::parse(root, &args).expect("parse must succeed");
        assert!(config.summary, "expected summary == true");
    }
}
