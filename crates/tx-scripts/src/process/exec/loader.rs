//! ELF parser binding (Phase 4 of the ELF-loader plan).
//!
//! Pure parsing logic: turns a kernel-owned `&[u8]` of ELF bytes into
//! an `ExecImagePlan` consumed by sibling Phase 1A
//! (`vm::scripts::build_aspace_from_image`) and sibling Phase 3
//! (`build_initial_user_stack`). No async, no I/O, no kernel state.
//!
//! Doc anchors:
//!   txdoc:EXEC-8-PHASE-3-LOAD-EXECUTABLE-IMAGE-PLAN
//!   txdoc:EXEC-8-4-HEADER-VALIDATION
//!   txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION
//!   txdoc:EXEC-8-6-AT-PHDR-COMPUTATION
//!
//! Supported ELF types:
//!   - `ET_EXEC`: static executables (load_bias = 0, absolute VAs), plus the
//!     staged dynamic shape where `PT_INTERP` identifies an interpreter and
//!     `PT_DYNAMIC` is left for that interpreter to consume.
//!   - `ET_DYN`: static-PIE and interpreter/shared-object images. Parsing uses
//!     `ET_DYN_LOAD_BIAS` as an initial bias; exec's combined-layout selector
//!     later rebases the image to its final address.
//!
//! Out of scope for the slice:
//!   - Kernel-side relocation processing and `DT_NEEDED` dependency loading.
//!   - relocations and debug info.
//!   - elf32 (RV64 only).
//!
//! Concrete decoder types do not escape this module. The architectural
//! contract is `ElfFileParser` plus the txKernel-owned `ExecImagePlan` output.

use alloc::vec::Vec;

mod elf08;
mod model;
mod parser;
mod policy;

pub use elf08::Elf08Parser;
pub use model::{
    ElfClass, ElfDecodeError, ElfEndian, ElfHeader, ElfLayoutError, ElfProgramHeader, ImageRange,
    StackRequest, TlsTemplate,
};
pub use parser::ElfFileParser;
pub use policy::ElfLoadPolicy;

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EV_CURRENT: u32 = 1;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
pub(crate) const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_TLS: u32 = 7;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// PT_GNU_STACK program header type.
const PT_GNU_STACK: u32 = 0x6474_e551;

/// PT_GNU_RELRO program header type.
const PT_GNU_RELRO: u32 = 0x6474_e552;

/// ELF64 program-header size, in bytes. (Elf64_Phdr is 56 bytes.)
pub const ELF64_PHENT: u64 = 56;
const USER_PAGE_ALIGNMENT: u64 = 4096;

/// Maximum accepted size of one `PT_INTERP` string, including its NUL.
pub const MAX_INTERP_PATH: u64 = 4096;

/// Provisional load bias applied while parsing `ET_DYN` images.
///
/// The combined-layout selector replaces this with the final load bias. This
/// value must be page-aligned so the ELF congruence invariant is preserved in
/// the intermediate plan as well.
pub(crate) const ET_DYN_LOAD_BIAS: u64 = 0x10000;

const EM_RISCV: u16 = 243;
const EM_LOONGARCH: u16 = 258;

/// Minimum ELF64 header size (`Elf64_Ehdr`), in bytes.
const ELF64_EHDR_SIZE: usize = 64;

/// Reasons `parse_image_plan` may reject an ELF byte slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Kernel-owned plan allocation failed.
    OutOfMemory,
    /// ELF magic missing or wrong architecture class / endianness /
    /// version.
    Magic,
    /// `e_machine` is not an architecture txKernel can enter.
    Arch,
    /// `e_type` is neither `ET_EXEC` nor `ET_DYN`.
    Type,
    /// `PT_DYNAMIC` was found without a `PT_INTERP` owner in an
    /// `ET_EXEC` binary. Dynamic executables must enter through their
    /// interpreter.
    HasInterp,
    /// At least one `PT_LOAD` is required.
    NoLoad,
    /// Program-header table malformed: header range overflows the
    /// byte slice, `e_phentsize` is wrong, or per-phdr decoding
    /// failed.
    Phdr,
    /// `PT_LOAD` segment has invalid alignment, congruence, sizes,
    /// final virtual range, entry coverage, or overlap.
    LoadSegment,
}

/// Selects policy differences between the requested image and its loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRole {
    Main,
    Interpreter,
}

/// Permission bits derived from a LOAD segment's `p_flags` (PF_R / PF_W
/// / PF_X). W+X is recorded rather than rejected, matching Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFlags {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// One PT_LOAD segment as seen by the parser. Pure parser output —
/// no `Cap<PageContainer>` here; Phase 5 (`exec_script`) composes
/// ELF parsing with backing-page construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadSegment {
    /// Virtual address (load_bias is 0 for `ET_EXEC`, so this is
    /// `p_vaddr` directly).
    pub vaddr: u64,
    /// `p_memsz` — total in-memory footprint.
    pub memsz: u64,
    /// `p_filesz` — bytes copied from the file (must be ≤ memsz).
    pub filesz: u64,
    /// `p_offset` — byte offset of segment data inside the ELF
    /// file. Phase 5 wires this to `read_exact_at(rnode, file_offset
    /// + k, dst, ...)` when materialising backing pages.
    pub file_offset: u64,
    pub flags: SegmentFlags,
    /// `p_align` — 0/1 mean no requirement; larger values are powers of two.
    pub align: u64,
}

/// Trailing BSS region. The page-rounded
/// `[vaddr + filesz, vaddr + memsz)` of the LOAD segment that
/// extends past file-backed bytes; backed by anonymous-private
/// pages by sibling Phase 1A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BssTail {
    pub vaddr: u64,
    pub size: u64,
}

/// txKernel-owned image plan produced from a parsed ELF. The output
/// is pure data — no kernel handles, no caps, no async machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecImagePlan {
    pub entry: u64,
    /// AT_PHDR — virtual address where program headers will live in
    /// the loaded image. Derived from the first LOAD whose file range
    /// contains the program-header table offset (per `EXEC-8-6`).
    /// `PT_PHDR` declarations do not override this mapping.
    pub at_phdr: u64,
    /// AT_PHENT — size of one program header (always 56 for ELF64).
    pub at_phent: u64,
    /// AT_PHNUM.
    pub at_phnum: u64,
    pub load_segments: Vec<LoadSegment>,
    /// Optional BSS extension (the last writable LOAD's
    /// `memsz > filesz` tail).
    pub bss_extension: Option<BssTail>,
    /// Load bias applied to this image's virtual addresses.
    /// `0` for `ET_EXEC`; initially `ET_DYN_LOAD_BIAS` for `ET_DYN`, then
    /// replaced by the combined-layout selector's final bias.
    /// All `vaddr` fields in `load_segments`, `entry`, and `at_phdr`
    /// already have this value added — callers see final in-memory VAs.
    pub load_bias: u64,
    /// Initial-exec TLS template, if the image declares `PT_TLS`.
    pub tls: Option<TlsTemplate>,
    /// `PT_DYNAMIC` location for the userspace interpreter. The kernel does
    /// not decode dynamic tags, symbols, or relocations.
    pub dynamic: Option<ImageRange>,
    /// Range that a userspace interpreter may protect after relocation.
    pub relro: Option<ImageRange>,
    /// Stack permission requested by `PT_GNU_STACK`; absent means NX.
    pub stack: StackRequest,
    /// Interpreter plan (from PT_INTERP).  None for static binaries.
    pub interpreter_path: Option<Vec<u8>>,
}

impl ExecImagePlan {
    /// Consume this plan and atomically return a fully rebased copy.
    ///
    /// Every address-bearing field is translated from `load_bias` to
    /// `new_bias` with checked unsigned arithmetic. On error the consumed plan
    /// is dropped, so callers cannot observe a partially rebased image.
    pub fn checked_rebase(mut self, new_bias: u64, user_top: u64) -> Result<Self, ElfLayoutError> {
        let required_align =
            self.load_segments
                .iter()
                .try_fold(USER_PAGE_ALIGNMENT, |required, segment| {
                    let align = segment.align.max(USER_PAGE_ALIGNMENT);
                    if !align.is_power_of_two() {
                        return Err(ElfLayoutError::InvalidAlignment);
                    }
                    Ok(required.max(align))
                })?;
        if !new_bias.is_multiple_of(required_align) {
            return Err(ElfLayoutError::InvalidAlignment);
        }
        let old_bias = self.load_bias;
        let rebase = |address: u64| {
            address
                .checked_sub(old_bias)
                .and_then(|relative| relative.checked_add(new_bias))
                .ok_or(ElfLayoutError::AddressOverflow)
        };

        self.entry = rebase(self.entry)?;
        self.at_phdr = rebase(self.at_phdr)?;
        for segment in &mut self.load_segments {
            segment.vaddr = rebase(segment.vaddr)?;
        }
        if let Some(bss) = &mut self.bss_extension {
            bss.vaddr = rebase(bss.vaddr)?;
        }
        if let Some(tls) = &mut self.tls {
            tls.vaddr = rebase(tls.vaddr)?;
        }
        if let Some(dynamic) = &mut self.dynamic {
            dynamic.vaddr = rebase(dynamic.vaddr)?;
        }
        if let Some(relro) = &mut self.relro {
            relro.vaddr = rebase(relro.vaddr)?;
        }
        self.load_bias = new_bias;

        let phdr_bytes = self
            .at_phent
            .checked_mul(self.at_phnum)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        let phdr_end = self
            .at_phdr
            .checked_add(phdr_bytes)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        if self.entry >= user_top || self.at_phdr >= user_top || phdr_end > user_top {
            return Err(ElfLayoutError::UserRange);
        }
        for segment in &self.load_segments {
            let end = segment
                .vaddr
                .checked_add(segment.memsz)
                .ok_or(ElfLayoutError::AddressOverflow)?;
            if end > user_top {
                return Err(ElfLayoutError::UserRange);
            }
        }
        for range in [self.dynamic, self.relro].into_iter().flatten() {
            let end = range
                .vaddr
                .checked_add(range.size)
                .ok_or(ElfLayoutError::AddressOverflow)?;
            if end > user_top {
                return Err(ElfLayoutError::UserRange);
            }
        }
        if let Some(tls) = self.tls {
            let end = tls
                .vaddr
                .checked_add(tls.memory_size)
                .ok_or(ElfLayoutError::AddressOverflow)?;
            if end > user_top {
                return Err(ElfLayoutError::UserRange);
            }
        }
        Ok(self)
    }
}

/// Parse the ELF header + program headers and produce an image plan.
///
/// This compatibility facade selects an architecture policy from the decoded
/// machine field. Platform-aware callers should use `parse_image_plan_with`
/// and `ElfLoadPolicy::for_platform` so cross-ISA images are rejected.
///
/// Cites: txdoc:EXEC-8-PHASE-3-LOAD-EXECUTABLE-IMAGE-PLAN.
pub fn parse_image_plan(elf_bytes: &[u8]) -> Result<ExecImagePlan, ParseError> {
    let header = Elf08Parser::parse_header(elf_bytes).map_err(map_header_decode_error)?;
    let arch = match header.machine {
        EM_LOONGARCH => tx_hal::Arch::LoongArch64,
        _ => tx_hal::Arch::Riscv64,
    };
    parse_image_plan_with::<Elf08Parser>(elf_bytes, ElfLoadPolicy::fixture(arch, u64::MAX))
}

/// Builds an image plan through a replaceable syntax decoder and explicit
/// platform policy. Only txKernel-owned ELF facts cross this boundary.
pub fn parse_image_plan_with<P: ElfFileParser>(
    elf_bytes: &[u8],
    policy: ElfLoadPolicy,
) -> Result<ExecImagePlan, ParseError> {
    let header = P::parse_header(elf_bytes).map_err(map_header_decode_error)?;
    validate_header(&header, policy)?;
    let table_len = program_header_table_len(&header, policy)?;
    let table_end = header
        .phoff
        .checked_add(table_len)
        .ok_or(ParseError::Phdr)?;
    let table_start = usize::try_from(header.phoff).map_err(|_| ParseError::Phdr)?;
    let table_end = usize::try_from(table_end).map_err(|_| ParseError::Phdr)?;
    let table_bytes = elf_bytes
        .get(table_start..table_end)
        .ok_or(ParseError::Phdr)?;
    let phdrs = P::parse_program_headers(&header, table_bytes).map_err(map_phdr_decode_error)?;
    if phdrs.len() != usize::from(header.phnum) {
        return Err(ParseError::Phdr);
    }

    let mut interp_phdr = None;
    for phdr in &phdrs {
        if phdr.p_type == PT_INTERP && interp_phdr.replace(*phdr).is_some() {
            return Err(ParseError::HasInterp);
        }
    }
    let interpreter_path = interp_phdr
        .as_ref()
        .map(|phdr| extract_interpreter_path(elf_bytes, phdr))
        .transpose()?;

    build_image_plan(
        &header,
        &phdrs,
        interpreter_path,
        elf_bytes.len() as u64,
        policy,
        ImageRole::Main,
    )
}

/// Builds a parser-independent image plan from already decoded ELF facts.
///
/// `file_len` is the executable's real length, not the size of any targeted
/// read window. The optional interpreter path is read separately by the
/// staged image reader.
pub fn build_image_plan(
    header: &ElfHeader,
    phdrs: &[ElfProgramHeader],
    interpreter_path: Option<Vec<u8>>,
    file_len: u64,
    policy: ElfLoadPolicy,
    role: ImageRole,
) -> Result<ExecImagePlan, ParseError> {
    validate_header(header, policy)?;
    if phdrs.len() != usize::from(header.phnum) {
        return Err(ParseError::Phdr);
    }
    let table_len = program_header_table_len(header, policy)?;
    let phnum = u64::from(header.phnum);
    let phent = u64::from(header.phentsize);
    header
        .phoff
        .checked_add(table_len)
        .filter(|end| *end <= file_len)
        .ok_or(ParseError::Phdr)?;

    let is_dyn = match header.elf_type {
        ET_EXEC => false,
        ET_DYN => true,
        _ => return Err(ParseError::Type),
    };

    let mut load_segments = Vec::new();
    load_segments
        .try_reserve_exact(phdrs.len())
        .map_err(|_| ParseError::OutOfMemory)?;
    let mut interp_phdr = None;
    let mut tls_phdr = None;
    let mut dynamic_phdr = None;
    let mut relro_phdrs = Vec::new();
    relro_phdrs
        .try_reserve_exact(phdrs.len())
        .map_err(|_| ParseError::OutOfMemory)?;
    let mut stack = StackRequest::default();

    for phdr in phdrs {
        match phdr.p_type {
            PT_INTERP => {
                if role == ImageRole::Interpreter
                    || !policy.allow_interpreter
                    || interp_phdr.replace(*phdr).is_some()
                {
                    return Err(ParseError::HasInterp);
                }
            }
            // The ELF dynamic table is a singleton; retaining one of several
            // candidates would make interpreter input depend on header order.
            PT_DYNAMIC => set_singleton(&mut dynamic_phdr, *phdr)?,
            // Linux derives AT_PHDR from the PT_LOAD mapping that contains
            // e_phoff. PT_PHDR is descriptive and does not add a rejection
            // condition when it is absent, duplicated, or inconsistent.
            PT_PHDR => {}
            PT_LOAD => {
                validate_declared_file_range(phdr, file_len, ParseError::LoadSegment)?;
                load_segments.push(translate_load(phdr, policy.page_size)?);
            }
            PT_TLS => set_singleton(&mut tls_phdr, *phdr)?,
            // Linux walks all GNU_STACK headers; the last request wins. The
            // remaining fields do not describe an image range.
            PT_GNU_STACK => {
                stack.executable_requested = phdr.p_flags & PF_X != 0;
            }
            PT_GNU_RELRO => relro_phdrs.push(*phdr),
            _ => validate_declared_file_range(phdr, file_len, ParseError::Phdr)?,
        }
    }

    if interp_phdr.is_some() != interpreter_path.is_some() {
        return Err(ParseError::HasInterp);
    }
    if let (Some(phdr), Some(path)) = (interp_phdr, interpreter_path.as_deref()) {
        validate_declared_file_range(&phdr, file_len, ParseError::Phdr)?;
        validate_interpreter_path(path)?;
        let encoded_len = u64::try_from(path.len())
            .ok()
            .and_then(|len| len.checked_add(1))
            .ok_or(ParseError::Phdr)?;
        if phdr.p_filesz != encoded_len || phdr.p_filesz > MAX_INTERP_PATH {
            return Err(ParseError::Phdr);
        }
    }

    if load_segments.is_empty() {
        return Err(ParseError::NoLoad);
    }
    if !is_dyn && dynamic_phdr.is_some() && interpreter_path.is_none() {
        return Err(ParseError::HasInterp);
    }

    let load_bias = compute_load_bias(is_dyn, policy.page_size, &load_segments)?;
    let tls = tls_phdr
        .map(|phdr| translate_tls(&phdr, load_bias, policy, file_len, &load_segments))
        .transpose()?;
    let dynamic = dynamic_phdr
        .map(|phdr| translate_dynamic(&phdr, load_bias, policy, file_len, &load_segments))
        .transpose()?;
    let relro = translate_relro_ranges(&relro_phdrs, load_bias, policy, file_len, &load_segments)?;
    let at_phdr = compute_at_phdr(&header, &load_segments)?
        .checked_add(load_bias)
        .ok_or(ParseError::Phdr)?;
    let entry = header
        .entry
        .checked_add(load_bias)
        .ok_or(ParseError::LoadSegment)?;

    for segment in &mut load_segments {
        segment.vaddr = segment
            .vaddr
            .checked_add(load_bias)
            .ok_or(ParseError::LoadSegment)?;
        validate_final_load_range(segment, policy)?;
    }

    detect_overlap(&load_segments, policy.page_size)?;
    let entry_is_executable = load_segments.iter().any(|segment| {
        segment.flags.executable
            && segment
                .vaddr
                .checked_add(segment.memsz)
                .is_some_and(|end| segment.vaddr <= entry && entry < end)
    });
    if entry >= policy.user_top || !entry_is_executable {
        return Err(ParseError::LoadSegment);
    }

    let at_phdr_end = at_phdr.checked_add(table_len).ok_or(ParseError::Phdr)?;
    if at_phdr >= policy.user_top || at_phdr_end > policy.user_top {
        return Err(ParseError::Phdr);
    }

    let bss_extension = compute_bss_extension(&load_segments)?;
    Ok(ExecImagePlan {
        entry,
        at_phdr,
        at_phent: phent,
        at_phnum: phnum,
        load_segments,
        bss_extension,
        load_bias,
        tls,
        dynamic,
        relro,
        stack,
        interpreter_path,
    })
}

pub(crate) fn validate_header(header: &ElfHeader, policy: ElfLoadPolicy) -> Result<(), ParseError> {
    if header.class != ElfClass::Elf64
        || header.endian != ElfEndian::Little
        || header.version != EV_CURRENT
        || header.ehsize as usize != ELF64_EHDR_SIZE
    {
        return Err(ParseError::Magic);
    }
    let expected_machine = match policy.arch {
        tx_hal::Arch::Riscv64 => EM_RISCV,
        tx_hal::Arch::LoongArch64 => EM_LOONGARCH,
    };
    if header.machine != expected_machine {
        return Err(ParseError::Arch);
    }
    if !policy.accepts_elf_flags(header.flags) {
        return Err(ParseError::Arch);
    }
    if !matches!(header.elf_type, ET_EXEC | ET_DYN) {
        return Err(ParseError::Type);
    }
    if policy.page_size == 0 || !policy.page_size.is_power_of_two() {
        return Err(ParseError::LoadSegment);
    }
    if header.phentsize as u64 != ELF64_PHENT
        || header.phnum == 0
        || header.phnum > policy.max_phdrs
    {
        return Err(ParseError::Phdr);
    }
    Ok(())
}

pub(crate) fn program_header_table_len(
    header: &ElfHeader,
    policy: ElfLoadPolicy,
) -> Result<u64, ParseError> {
    if header.phnum == 0
        || header.phnum > policy.max_phdrs
        || header.phentsize as u64 != ELF64_PHENT
    {
        return Err(ParseError::Phdr);
    }
    u64::from(header.phnum)
        .checked_mul(u64::from(header.phentsize))
        .ok_or(ParseError::Phdr)
}

// ----------------------------------------------------------------------
// Helpers

fn set_singleton(
    slot: &mut Option<ElfProgramHeader>,
    phdr: ElfProgramHeader,
) -> Result<(), ParseError> {
    if slot.replace(phdr).is_some() {
        return Err(ParseError::Phdr);
    }
    Ok(())
}

fn translate_tls(
    phdr: &ElfProgramHeader,
    load_bias: u64,
    policy: ElfLoadPolicy,
    file_len: u64,
    load_segments: &[LoadSegment],
) -> Result<TlsTemplate, ParseError> {
    let vaddr = validate_metadata_phdr(phdr, load_bias, policy, file_len)?;
    require_file_mapping_coverage(phdr.p_offset, phdr.p_vaddr, phdr.p_filesz, load_segments)?;
    require_memory_load_coverage(
        phdr.p_vaddr
            .checked_add(phdr.p_filesz)
            .ok_or(ParseError::Phdr)?,
        phdr.p_memsz - phdr.p_filesz,
        load_segments,
    )?;
    Ok(TlsTemplate {
        vaddr,
        file_offset: phdr.p_offset,
        file_size: phdr.p_filesz,
        memory_size: phdr.p_memsz,
        align: phdr.p_align,
    })
}

fn translate_dynamic(
    phdr: &ElfProgramHeader,
    load_bias: u64,
    policy: ElfLoadPolicy,
    file_len: u64,
    load_segments: &[LoadSegment],
) -> Result<ImageRange, ParseError> {
    let vaddr = validate_metadata_phdr(phdr, load_bias, policy, file_len)?;
    require_file_mapping_coverage(phdr.p_offset, phdr.p_vaddr, phdr.p_filesz, load_segments)?;
    require_memory_load_coverage(
        phdr.p_vaddr
            .checked_add(phdr.p_filesz)
            .ok_or(ParseError::Phdr)?,
        phdr.p_memsz - phdr.p_filesz,
        load_segments,
    )?;
    Ok(ImageRange {
        vaddr,
        size: phdr.p_memsz,
    })
}

fn translate_relro_ranges(
    phdrs: &[ElfProgramHeader],
    load_bias: u64,
    policy: ElfLoadPolicy,
    file_len: u64,
    load_segments: &[LoadSegment],
) -> Result<Option<ImageRange>, ParseError> {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(phdrs.len())
        .map_err(|_| ParseError::OutOfMemory)?;
    for phdr in phdrs {
        let vaddr = validate_metadata_phdr(phdr, load_bias, policy, file_len)?;
        require_memory_load_coverage(phdr.p_vaddr, phdr.p_memsz, load_segments)?;
        ranges.push(ImageRange {
            vaddr,
            size: phdr.p_memsz,
        });
    }
    ranges.sort_unstable_by_key(|range| range.vaddr);
    let Some(mut merged) = ranges.first().copied() else {
        return Ok(None);
    };
    for range in &ranges[1..] {
        let merged_end = merged
            .vaddr
            .checked_add(merged.size)
            .ok_or(ParseError::Phdr)?;
        if range.vaddr > merged_end {
            // One ImageRange cannot represent a hole without asking the
            // interpreter to protect unrelated memory.
            return Err(ParseError::Phdr);
        }
        let range_end = range
            .vaddr
            .checked_add(range.size)
            .ok_or(ParseError::Phdr)?;
        merged.size = merged_end.max(range_end) - merged.vaddr;
    }
    Ok(Some(merged))
}

fn validate_metadata_phdr(
    phdr: &ElfProgramHeader,
    load_bias: u64,
    policy: ElfLoadPolicy,
    file_len: u64,
) -> Result<u64, ParseError> {
    if phdr.p_filesz > phdr.p_memsz || (phdr.p_align > 1 && !phdr.p_align.is_power_of_two()) {
        return Err(ParseError::Phdr);
    }
    validate_declared_file_range(phdr, file_len, ParseError::Phdr)?;
    validate_final_memory_range(phdr, load_bias, policy)
}

fn validate_final_memory_range(
    phdr: &ElfProgramHeader,
    load_bias: u64,
    policy: ElfLoadPolicy,
) -> Result<u64, ParseError> {
    phdr.p_vaddr
        .checked_add(phdr.p_memsz)
        .ok_or(ParseError::Phdr)?;
    let vaddr = phdr
        .p_vaddr
        .checked_add(load_bias)
        .ok_or(ParseError::Phdr)?;
    let end = vaddr.checked_add(phdr.p_memsz).ok_or(ParseError::Phdr)?;
    if vaddr >= policy.user_top || end > policy.user_top {
        return Err(ParseError::Phdr);
    }
    Ok(vaddr)
}

fn require_memory_load_coverage(
    start: u64,
    size: u64,
    load_segments: &[LoadSegment],
) -> Result<(), ParseError> {
    let end = start.checked_add(size).ok_or(ParseError::Phdr)?;
    let mut covered = start;
    while covered < end {
        let mut next = covered;
        for segment in load_segments {
            let segment_start = segment.vaddr;
            let segment_end = segment_start
                .checked_add(segment.memsz)
                .ok_or(ParseError::Phdr)?;
            if segment_start <= covered && covered < segment_end {
                next = next.max(segment_end.min(end));
            }
        }
        if next == covered {
            return Err(ParseError::Phdr);
        }
        covered = next;
    }
    Ok(())
}

fn require_file_mapping_coverage(
    file_offset: u64,
    vaddr: u64,
    size: u64,
    load_segments: &[LoadSegment],
) -> Result<(), ParseError> {
    file_offset.checked_add(size).ok_or(ParseError::Phdr)?;
    vaddr.checked_add(size).ok_or(ParseError::Phdr)?;
    let mut covered = 0;
    while covered < size {
        let file_pos = file_offset.checked_add(covered).ok_or(ParseError::Phdr)?;
        let memory_pos = vaddr.checked_add(covered).ok_or(ParseError::Phdr)?;
        let mut next = covered;
        for segment in load_segments {
            let file_end = segment
                .file_offset
                .checked_add(segment.filesz)
                .ok_or(ParseError::Phdr)?;
            if !(segment.file_offset <= file_pos && file_pos < file_end) {
                continue;
            }
            let mapped_vaddr = segment
                .vaddr
                .checked_add(file_pos - segment.file_offset)
                .ok_or(ParseError::Phdr)?;
            if mapped_vaddr != memory_pos {
                continue;
            }
            let available = file_end - file_pos;
            next = next.max(
                covered
                    .checked_add(available.min(size - covered))
                    .ok_or(ParseError::Phdr)?,
            );
        }
        if next == covered {
            return Err(ParseError::Phdr);
        }
        covered = next;
    }
    Ok(())
}

fn compute_load_bias(
    is_dyn: bool,
    page_size: u64,
    load_segments: &[LoadSegment],
) -> Result<u64, ParseError> {
    if !is_dyn {
        return Ok(0);
    }
    let alignment = load_segments
        .iter()
        .map(|segment| segment.align)
        .fold(page_size, u64::max);
    let mask = alignment - 1;
    ET_DYN_LOAD_BIAS
        .checked_add(mask)
        .map(|value| value & !mask)
        .ok_or(ParseError::LoadSegment)
}

fn validate_declared_file_range(
    phdr: &ElfProgramHeader,
    file_len: u64,
    error: ParseError,
) -> Result<(), ParseError> {
    let end = phdr.p_offset.checked_add(phdr.p_filesz).ok_or(error)?;
    if end > file_len {
        return Err(error);
    }
    Ok(())
}

/// Converts and validates one parser-independent `PT_LOAD` record.
fn translate_load(phdr: &ElfProgramHeader, page_size: u64) -> Result<LoadSegment, ParseError> {
    // §8.5 program-header validation.
    if phdr.p_filesz > phdr.p_memsz {
        return Err(ParseError::LoadSegment);
    }
    // Overflow checks.
    phdr.p_offset
        .checked_add(phdr.p_filesz)
        .ok_or(ParseError::LoadSegment)?;
    phdr.p_vaddr
        .checked_add(phdr.p_memsz)
        .ok_or(ParseError::LoadSegment)?;

    // ELF permits 0/1 to mean "no alignment requirement". Other values
    // describe object alignment and need only be powers of two; page-level
    // mapping congruence is validated separately below.
    let align = phdr.p_align;
    if align > 1 && !align.is_power_of_two() {
        return Err(ParseError::LoadSegment);
    }

    // Linux requires page congruence; larger p_align values do not impose an
    // additional kernel mapping constraint.
    if phdr.p_vaddr % page_size != phdr.p_offset % page_size {
        return Err(ParseError::LoadSegment);
    }

    // Reject prot == 0. Linux records combined W+X permissions rather than
    // rejecting an otherwise valid LOAD segment.
    let readable = phdr.p_flags & PF_R != 0;
    let writable = phdr.p_flags & PF_W != 0;
    let executable = phdr.p_flags & PF_X != 0;
    if !(readable || writable || executable) {
        return Err(ParseError::LoadSegment);
    }
    Ok(LoadSegment {
        vaddr: phdr.p_vaddr,
        memsz: phdr.p_memsz,
        filesz: phdr.p_filesz,
        file_offset: phdr.p_offset,
        flags: SegmentFlags {
            readable,
            writable,
            executable,
        },
        align,
    })
}

/// Page-floor / page-ceil helpers operating on `u64`.
fn page_floor(x: u64, page_size: u64) -> u64 {
    x & !(page_size - 1)
}
fn page_ceil(x: u64, page_size: u64) -> Option<u64> {
    let mask = page_size - 1;
    x.checked_add(mask).map(|v| v & !mask)
}

fn detect_overlap(segments: &[LoadSegment], page_size: u64) -> Result<(), ParseError> {
    // Quadratic scan; LOAD count is tiny (≤ 16 for real binaries,
    // ≤ MAX_PHDRS=64 by header validation).
    for i in 0..segments.len() {
        let a_start = page_floor(segments[i].vaddr, page_size);
        let a_end = page_ceil(
            segments[i]
                .vaddr
                .checked_add(segments[i].memsz)
                .ok_or(ParseError::LoadSegment)?,
            page_size,
        )
        .ok_or(ParseError::LoadSegment)?;
        if a_end <= a_start {
            // Zero-size or wrapped — reject.
            return Err(ParseError::LoadSegment);
        }
        for b in &segments[i + 1..] {
            let b_start = page_floor(b.vaddr, page_size);
            let b_end = page_ceil(
                b.vaddr
                    .checked_add(b.memsz)
                    .ok_or(ParseError::LoadSegment)?,
                page_size,
            )
            .ok_or(ParseError::LoadSegment)?;
            if a_start < b_end && b_start < a_end {
                return Err(ParseError::LoadSegment);
            }
        }
    }
    Ok(())
}

fn compute_at_phdr(header: &ElfHeader, load_segments: &[LoadSegment]) -> Result<u64, ParseError> {
    let phoff = header.phoff;
    for seg in load_segments {
        let seg_file_end = seg
            .file_offset
            .checked_add(seg.filesz)
            .ok_or(ParseError::Phdr)?;
        if seg.file_offset <= phoff && phoff < seg_file_end {
            return seg
                .vaddr
                .checked_add(phoff - seg.file_offset)
                .ok_or(ParseError::Phdr);
        }
    }
    Err(ParseError::Phdr)
}

fn extract_interpreter_path(
    elf_bytes: &[u8],
    phdr: &ElfProgramHeader,
) -> Result<Vec<u8>, ParseError> {
    let len = phdr.p_filesz;
    if len == 0 || len > MAX_INTERP_PATH {
        return Err(ParseError::Phdr);
    }
    let end = phdr.p_offset.checked_add(len).ok_or(ParseError::Phdr)?;
    let start = usize::try_from(phdr.p_offset).map_err(|_| ParseError::Phdr)?;
    let end = usize::try_from(end).map_err(|_| ParseError::Phdr)?;
    let bytes = elf_bytes.get(start..end).ok_or(ParseError::Phdr)?;
    parse_interpreter_path(bytes)
}

pub(crate) fn parse_interpreter_path(bytes: &[u8]) -> Result<Vec<u8>, ParseError> {
    let Some((&0, path)) = bytes.split_last() else {
        return Err(ParseError::Phdr);
    };
    validate_interpreter_path(path)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(path.len())
        .map_err(|_| ParseError::OutOfMemory)?;
    owned.extend_from_slice(path);
    Ok(owned)
}

fn validate_interpreter_path(path: &[u8]) -> Result<(), ParseError> {
    let encoded_len = u64::try_from(path.len())
        .ok()
        .and_then(|len| len.checked_add(1))
        .ok_or(ParseError::Phdr)?;
    if path.first() != Some(&b'/') || path.contains(&0) || encoded_len > MAX_INTERP_PATH {
        return Err(ParseError::Phdr);
    }
    Ok(())
}

fn validate_final_load_range(
    segment: &LoadSegment,
    policy: ElfLoadPolicy,
) -> Result<(), ParseError> {
    let exact_end = segment
        .vaddr
        .checked_add(segment.memsz)
        .ok_or(ParseError::LoadSegment)?;
    let start = page_floor(segment.vaddr, policy.page_size);
    let end = page_ceil(exact_end, policy.page_size).ok_or(ParseError::LoadSegment)?;
    if end <= start || end > policy.user_top {
        return Err(ParseError::LoadSegment);
    }
    Ok(())
}

fn map_header_decode_error(error: ElfDecodeError) -> ParseError {
    match error {
        ElfDecodeError::OutOfMemory => ParseError::OutOfMemory,
        ElfDecodeError::BadMagic
        | ElfDecodeError::UnsupportedClass
        | ElfDecodeError::UnsupportedEndian
        | ElfDecodeError::UnsupportedVersion
        | ElfDecodeError::Truncated => ParseError::Magic,
        ElfDecodeError::BadEntrySize
        | ElfDecodeError::IntegerOverflow
        | ElfDecodeError::Malformed => ParseError::Phdr,
    }
}

fn map_phdr_decode_error(error: ElfDecodeError) -> ParseError {
    match error {
        ElfDecodeError::OutOfMemory => ParseError::OutOfMemory,
        _ => ParseError::Phdr,
    }
}

/// Returns the BSS extension for the last LOAD segment whose
/// `memsz > filesz`. Multiple BSS-extending LOADs are legal ELF
/// (e.g. LA64 busybox has two: .relro_padding and .data/.bss).
/// The VM mapper handles BSS per-segment; this keeps the last tail
/// for auxv / debug consumers.
fn compute_bss_extension(load_segments: &[LoadSegment]) -> Result<Option<BssTail>, ParseError> {
    let mut found: Option<BssTail> = None;
    for seg in load_segments {
        if seg.memsz > seg.filesz {
            // BSS spans `[vaddr + filesz, vaddr + memsz)`. The page
            // walker will round this; here we report exact bytes.
            let tail_vaddr = seg
                .vaddr
                .checked_add(seg.filesz)
                .ok_or(ParseError::LoadSegment)?;
            let tail_size = seg.memsz - seg.filesz;
            // Multiple BSS-extending LOADs are legal (e.g. LA64
            // busybox has two: .relro_padding and .data/.bss).
            // `vm/scripts.rs` ignores `bss_extension` for actual
            // mapping (handled per-segment by `register_load_segment`);
            // keep the last one for auxv / debug consumers.
            found = Some(BssTail {
                vaddr: tail_vaddr,
                size: tail_size,
            });
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
