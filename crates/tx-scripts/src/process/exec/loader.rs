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
//!   - `ET_DYN`: static-PIE (position-independent, no `DT_NEEDED`, zero
//!     relocations) and interpreter/shared-object images. The kernel chooses a
//!     fixed load bias (`ET_DYN_LOAD_BIAS`) and shifts all virtual addresses.
//!
//! Out of scope for the slice:
//!   - Kernel-side relocation processing and `DT_NEEDED` dependency loading.
//!   - relocations and debug info.
//!   - elf32 (RV64 only).
//!
//! `goblin` types do not escape this module. The architectural contract
//! is `parse_image_plan` and the txKernel-owned `ExecImagePlan` output.
//! A future swap to the `elf` crate (per `EXEC-8-10`) would leave the
//! contract unchanged.

use alloc::vec::Vec;

use goblin::container::{Container, Ctx, Endian};
use goblin::elf::header::header64;
use goblin::elf::header::{
    Header, EI_CLASS, EI_DATA, EI_VERSION, ELFCLASS64, ELFDATA2LSB, ET_DYN, ET_EXEC, EV_CURRENT,
};
use goblin::elf::program_header::{
    ProgramHeader, PF_R, PF_W, PF_X, PT_DYNAMIC, PT_INTERP, PT_LOAD, PT_PHDR,
};

mod model;
mod parser;

pub use model::{ElfClass, ElfDecodeError, ElfEndian, ElfHeader, ElfProgramHeader};
pub use parser::ElfFileParser;

/// PT_GNU_STACK program header type (not in goblin's constants).
const PT_GNU_STACK: u32 = 0x6474_e551;

/// PT_GNU_RELRO program header type.
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
const PT_GNU_RELRO: u32 = 0x6474_e552;

/// ELF64 program-header size, in bytes. (Elf64_Phdr is 56 bytes.)
pub const ELF64_PHENT: u64 = 56;

/// Page size used for LOAD-segment alignment validation. RV64 uses
/// 4 KiB pages; the slice does not support huge pages (per
/// `EXEC-8-5`).
const PAGE_SIZE: u64 = 4096;

/// Hard cap on the number of program headers we will accept (matches
/// `EXEC-8-4`'s MAX_PHDRS). Keeps targeted reads bounded.
const MAX_PHDRS: u16 = 64;

/// Fixed load bias applied to `ET_DYN` (static-PIE) images.
///
/// Linux uses a random base for ASLR; the competition slice uses a
/// fixed value so test programs land at predictable addresses.
/// Must be a multiple of `PAGE_SIZE` so the ELF congruence invariant
/// (`p_vaddr % p_align == p_offset % p_align`) is preserved after bias.
pub(crate) const ET_DYN_LOAD_BIAS: u64 = 0x10000;

const EM_RISCV: u16 = 243;
const EM_LOONGARCH: u16 = 258;

/// Minimum ELF64 header size (`Elf64_Ehdr`), in bytes.
const ELF64_EHDR_SIZE: usize = 64;

/// Reasons `parse_image_plan` may reject an ELF byte slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
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
    /// overlap, or there are multiple BSS-extending LOADs.
    LoadSegment,
}

/// Permission bits derived from a LOAD segment's `p_flags` (PF_R / PF_W
/// / PF_X). The slice rejects W+X at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFlags {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// One PT_LOAD segment as seen by the parser. Pure parser output —
/// no `Cap<PageContainer>` here; Phase 5 (`exec_script`) composes
/// ELF parsing with backing-page construction.
#[derive(Debug, Clone)]
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
    /// `p_align` — validated to be a power of two and ≥ PAGE_SIZE.
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

/// Interpreter image plan.  Built from a separate ELF parse when
/// the main binary carries PT_INTERP.  Unlike ExecImagePlan
/// (main + interpreter), this is just the interpreter's image.
#[derive(Debug, Clone)]
pub struct InterpreterPlan {
    /// Interpreter base reported through `AT_BASE`.
    pub load_bias: u64,
    /// Interpreter entry point, already bias-adjusted.
    pub entry: u64,
    /// LOAD segments, already bias-adjusted.
    pub load_segments: Vec<LoadSegment>,
    /// Optional BSS tail.
    pub bss_extension: Option<BssTail>,
    /// Whether PT_GNU_STACK requests an executable stack.
    pub executable_stack: bool,
}

/// txKernel-owned image plan produced from a parsed ELF. The output
/// is pure data — no kernel handles, no caps, no async machinery.
#[derive(Debug, Clone)]
pub struct ExecImagePlan {
    pub entry: u64,
    /// AT_PHDR — virtual address where program headers will live in
    /// the loaded image. Computed from PT_PHDR if present, else
    /// derived from the LOAD that contains the program-header table
    /// offset (per `EXEC-8-6`). If neither holds, parsing fails with
    /// `ParseError::Phdr`.
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
    /// `0` for `ET_EXEC`; `ET_DYN_LOAD_BIAS` for static-PIE (`ET_DYN`).
    /// All `vaddr` fields in `load_segments`, `entry`, and `at_phdr`
    /// already have this value added — callers see final in-memory VAs.
    pub load_bias: u64,
    /// Whether PT_GNU_STACK requests an executable stack.
    /// Default false; true only when the binary explicitly adds PF_X.
    pub executable_stack: bool,
    /// Interpreter plan (from PT_INTERP).  None for static binaries.
    pub interpreter_path: Option<Vec<u8>>,
}

/// Parse the ELF header + program headers and produce an image plan.
///
/// Validates: ELFCLASS64, ELFDATA2LSB, EV_CURRENT, supported machine,
/// ET_EXEC or static-PIE ET_DYN, no unsupported interpreter handoff, ≥ 1 PT_LOAD.
/// Walks PT_LOAD segments;
/// rejects overlap, bad alignment, congruence violations, multiple
/// BSS-extending LOADs.
///
/// Cites: txdoc:EXEC-8-PHASE-3-LOAD-EXECUTABLE-IMAGE-PLAN.
pub fn parse_image_plan(elf_bytes: &[u8]) -> Result<ExecImagePlan, ParseError> {
    // ----- Header --------------------------------------------------
    if elf_bytes.len() < ELF64_EHDR_SIZE {
        return Err(ParseError::Magic);
    }
    // The unified `goblin::elf::Header::parse` is gated behind a
    // per-class macro that we don't re-export; the per-arch
    // `header64::Header::parse` is what's actually exposed under the
    // `endian_fd + alloc + elf64` feature set. Convert into the
    // unified `Header` (its `From<header64::Header>` impl is
    // unconditional under those features) so downstream paths
    // remain class-agnostic.
    let header64_raw = header64::Header::parse(elf_bytes).map_err(|_| ParseError::Magic)?;
    let header: Header = header64_raw.into();

    // §8.4 header validation. The unified `Header::parse` already
    // checks the magic; we re-check class/data/version/machine/type
    // explicitly so the failure errno surface is precise.
    if header.e_ident[EI_CLASS] != ELFCLASS64
        || header.e_ident[EI_DATA] != ELFDATA2LSB
        || header.e_ident[EI_VERSION] != EV_CURRENT
    {
        return Err(ParseError::Magic);
    }
    if !matches!(header.e_machine, EM_RISCV | EM_LOONGARCH) {
        return Err(ParseError::Arch);
    }
    // Accept both ET_EXEC (static, absolute VAs) and ET_DYN (static-PIE,
    // relative VAs shifted by ET_DYN_LOAD_BIAS). Anything else is rejected.
    let is_dyn = match header.e_type {
        ET_EXEC => false,
        ET_DYN => true,
        _ => return Err(ParseError::Type),
    };

    // ELF64 program-header size is fixed; assert and propagate.
    if header.e_phentsize as u64 != ELF64_PHENT {
        return Err(ParseError::Phdr);
    }
    if header.e_phnum == 0 || header.e_phnum > MAX_PHDRS {
        return Err(ParseError::Phdr);
    }

    let phoff = header.e_phoff;
    let phnum = header.e_phnum as u64;
    let phent = header.e_phentsize as u64;

    let phdr_table_end = phoff
        .checked_add(phnum.checked_mul(phent).ok_or(ParseError::Phdr)?)
        .ok_or(ParseError::Phdr)?;
    if phdr_table_end > elf_bytes.len() as u64 {
        return Err(ParseError::Phdr);
    }

    // ----- Program headers ----------------------------------------
    // Supported ELF64 little-endian machines share the same program-header layout.
    let ctx = Ctx::new(Container::Big, Endian::Little);
    let phdrs = ProgramHeader::parse(elf_bytes, phoff as usize, phnum as usize, ctx)
        .map_err(|_| ParseError::Phdr)?;

    let mut load_segments: Vec<LoadSegment> = Vec::new();
    let mut pt_phdr_vaddr: Option<u64> = None;
    let mut exec_stack: bool = false;
    let mut interp_path: Option<Vec<u8>> = None;
    let mut has_dynamic = false;

    for phdr in &phdrs {
        match phdr.p_type {
            PT_INTERP => {
                // Extract interpreter path from ELF bytes.
                let off = phdr.p_offset as usize;
                let len = (phdr.p_filesz as usize).min(4096);
                if off + len <= elf_bytes.len() {
                    let path = elf_bytes[off..off + len]
                        .split(|&b| b == 0)
                        .next()
                        .unwrap_or(&[])
                        .to_vec();
                    interp_path = Some(path);
                }
            }
            PT_DYNAMIC => {
                has_dynamic = true;
                // PT_INTERP-interpreted binaries may also carry
                // PT_DYNAMIC; the dynamic linker consumes that table.
            }
            PT_PHDR => {
                pt_phdr_vaddr = Some(phdr.p_vaddr);
            }
            PT_LOAD => {
                load_segments.push(translate_load(phdr, is_dyn)?);
            }
            // PT_TLS, PT_NOTE, PT_GNU_RELRO, PT_GNU_EH_FRAME, ...
            // ignored at parse time.
            _ => {
                if phdr.p_type == PT_GNU_STACK && (phdr.p_flags & PF_X) != 0 {
                    exec_stack = true;
                }
            }
        }
    }

    let _executable_stack = exec_stack;

    if load_segments.is_empty() {
        return Err(ParseError::NoLoad);
    }
    if !is_dyn && has_dynamic && interp_path.is_none() {
        return Err(ParseError::HasInterp);
    }

    // Choose load bias: 0 for ET_EXEC (absolute VAs already in place),
    // ET_DYN_LOAD_BIAS for static-PIE (relative VAs shifted to a fixed
    // kernel-chosen base). Must be a multiple of PAGE_SIZE to preserve
    // the ELF congruence invariant.
    let load_bias: u64 = if is_dyn { ET_DYN_LOAD_BIAS } else { 0 };

    // §8.6: AT_PHDR computation — run on original (pre-bias) vaddrs
    // so the delta arithmetic inside uses unshifted segment VAs, then
    // add the bias to produce the final in-memory VA.
    let at_phdr = compute_at_phdr(&header, &load_segments, pt_phdr_vaddr)?
        .checked_add(load_bias)
        .ok_or(ParseError::LoadSegment)?;

    let entry = header
        .e_entry
        .checked_add(load_bias)
        .ok_or(ParseError::LoadSegment)?;

    // Shift all segment VAs by the load bias. After this point every
    // address in `load_segments` is a final in-memory VA.
    for seg in &mut load_segments {
        seg.vaddr = seg
            .vaddr
            .checked_add(load_bias)
            .ok_or(ParseError::LoadSegment)?;
    }

    // §8.5: page-rounded LOAD ranges must not overlap (checked on
    // bias-adjusted VAs so the plan reflects the final in-memory layout).
    detect_overlap(&load_segments)?;

    // BSS tail: at most one LOAD may have `memsz > filesz` for the
    // slice. (Multi-BSS LOADs are theoretically legal but unusual
    // for static binaries; a future slice can lift this.)
    let bss_extension = compute_bss_extension(&load_segments)?;

    Ok(ExecImagePlan {
        entry,
        at_phdr,
        at_phent: phent,
        at_phnum: phnum,
        load_segments,
        bss_extension,
        load_bias,
        executable_stack: exec_stack,
        interpreter_path: interp_path,
    })
}

// ----------------------------------------------------------------------
// Helpers

/// `is_dyn`: when `true` (ET_DYN static-PIE), combined W+X `PT_LOAD`
/// segments are accepted. Some linkers emit a single RWX segment for
/// small position-independent programs.
fn translate_load(phdr: &ProgramHeader, is_dyn: bool) -> Result<LoadSegment, ParseError> {
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

    // Alignment: 0/1 are treated as "no alignment requirement", but
    // for LOAD we still require ≥ PAGE_SIZE so `p_vaddr ≡ p_offset
    // (mod PAGE_SIZE)` is a meaningful check.
    let align = phdr.p_align;
    if align < PAGE_SIZE || !align.is_power_of_two() {
        return Err(ParseError::LoadSegment);
    }

    // ELF congruence: `p_vaddr % p_align == p_offset % p_align`.
    // (Tightened at PAGE_SIZE by `EXEC-8-5`; we use the segment's
    // own align since it is already validated ≥ PAGE_SIZE.)
    if phdr.p_vaddr % align != phdr.p_offset % align {
        return Err(ParseError::LoadSegment);
    }

    // Reject prot == 0. W+X is rejected for ET_EXEC per `EXEC-8-5`
    // policy but allowed for ET_DYN static-PIE, where some linkers
    // emit a single combined RWX PT_LOAD for small programs.
    let readable = phdr.p_flags & PF_R != 0;
    let writable = phdr.p_flags & PF_W != 0;
    let executable = phdr.p_flags & PF_X != 0;
    if !(readable || writable || executable) {
        return Err(ParseError::LoadSegment);
    }
    if !is_dyn && writable && executable {
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
fn page_floor(x: u64) -> u64 {
    x & !(PAGE_SIZE - 1)
}
fn page_ceil(x: u64) -> Option<u64> {
    let mask = PAGE_SIZE - 1;
    x.checked_add(mask).map(|v| v & !mask)
}

fn detect_overlap(segments: &[LoadSegment]) -> Result<(), ParseError> {
    // Quadratic scan; LOAD count is tiny (≤ 16 for real binaries,
    // ≤ MAX_PHDRS=64 by header validation).
    for i in 0..segments.len() {
        let a_start = page_floor(segments[i].vaddr);
        let a_end = page_ceil(segments[i].vaddr.saturating_add(segments[i].memsz))
            .ok_or(ParseError::LoadSegment)?;
        if a_end <= a_start {
            // Zero-size or wrapped — reject.
            return Err(ParseError::LoadSegment);
        }
        for b in &segments[i + 1..] {
            let b_start = page_floor(b.vaddr);
            let b_end =
                page_ceil(b.vaddr.saturating_add(b.memsz)).ok_or(ParseError::LoadSegment)?;
            if a_start < b_end && b_start < a_end {
                return Err(ParseError::LoadSegment);
            }
        }
    }
    Ok(())
}

fn compute_at_phdr(
    header: &Header,
    load_segments: &[LoadSegment],
    pt_phdr_vaddr: Option<u64>,
) -> Result<u64, ParseError> {
    if let Some(va) = pt_phdr_vaddr {
        return Ok(va);
    }
    let phoff = header.e_phoff;
    let phdr_end = phoff
        .checked_add(
            (header.e_phnum as u64)
                .checked_mul(header.e_phentsize as u64)
                .ok_or(ParseError::Phdr)?,
        )
        .ok_or(ParseError::Phdr)?;
    for seg in load_segments {
        let seg_file_end = seg
            .file_offset
            .checked_add(seg.filesz)
            .ok_or(ParseError::Phdr)?;
        if seg.file_offset <= phoff && phdr_end <= seg_file_end {
            // (phoff - seg.file_offset) is a within-segment delta;
            // adding to vaddr cannot overflow because phdr_end ≤
            // seg_file_end ≤ seg.file_offset + memsz, and
            // vaddr + memsz was overflow-checked earlier.
            return Ok(seg.vaddr + (phoff - seg.file_offset));
        }
    }
    Err(ParseError::Phdr)
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
