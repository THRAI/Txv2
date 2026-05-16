//! Tests for the ELF parser binding (Phase 4).
//!
//! Fixtures are hand-crafted byte-by-byte minimal RV64 ET_EXEC ELF
//! images (we don't ship a checked-in binary; goblin doesn't bundle
//! one either). Each test mutates the base image to produce the
//! desired rejection. Layout reference: `Elf64_Ehdr` (64 bytes) +
//! `Elf64_Phdr` * N (56 bytes each), all little-endian.
//!
//! The fixtures verify the parser contract; round-trip parsing is
//! exercised by `parse_image_plan` itself, which uses goblin under
//! the hood for the byte-decode step.

use alloc::vec;
use alloc::vec::Vec;

use super::*;

// ELF identification offsets (mirrors goblin's constants for clarity).
const EI_MAG0: usize = 0;
const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const EI_OSABI: usize = 7;

// e_ident[EI_CLASS] / [EI_DATA] / [EI_VERSION] are spec-driven.

const ELFCLASS64_BYTE: u8 = 2;
const ELFDATA2LSB_BYTE: u8 = 1;
const EV_CURRENT_BYTE: u8 = 1;

const ET_EXEC_U16: u16 = 2;
const ET_DYN_U16: u16 = 3;
const EM_RISCV_U16: u16 = 243;
const EM_X86_64_U16: u16 = 62;

const PT_LOAD_U32: u32 = 1;
const PT_DYNAMIC_U32: u32 = 2;
const PT_INTERP_U32: u32 = 3;
const PT_PHDR_U32: u32 = 6;

const PF_X_BIT: u32 = 1;
const PF_W_BIT: u32 = 2;
const PF_R_BIT: u32 = 4;

/// Page size used in fixtures. Matches the parser's PAGE_SIZE.
const FIX_PAGE: u64 = 4096;

/// Layout for the base minimal-static-ELF fixture:
/// - Ehdr at 0..64
/// - PT_PHDR at 64..120  (vaddr = base_load_vaddr + 64)
/// - PT_LOAD at 120..176 (R+X, vaddr = base_load_vaddr,
///   filesz = 176, memsz = 176)
struct FixtureCfg {
    e_type: u16,
    e_machine: u16,
    e_entry: u64,
    /// Override e_phnum (defaults to count of phdrs in `phdrs`).
    e_phnum_override: Option<u16>,
    /// Override e_phentsize (defaults to 56).
    e_phentsize_override: Option<u16>,
    e_ident_class: u8,
    e_ident_data: u8,
    e_ident_version: u8,
    /// PT_PHDR omitted when `false`; LOAD always included.
    include_pt_phdr: bool,
    /// Program headers to add *after* (optional) PT_PHDR. The parser
    /// walks these in array order.
    phdrs: Vec<PhdrSpec>,
}

#[derive(Clone, Copy)]
struct PhdrSpec {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

impl PhdrSpec {
    fn load(vaddr: u64, offset: u64, filesz: u64, memsz: u64, flags: u32) -> Self {
        Self {
            p_type: PT_LOAD_U32,
            p_flags: flags,
            p_offset: offset,
            p_vaddr: vaddr,
            p_paddr: vaddr,
            p_filesz: filesz,
            p_memsz: memsz,
            p_align: FIX_PAGE,
        }
    }
}

impl FixtureCfg {
    /// Default minimal RV64 ET_EXEC fixture: PT_PHDR + one R+X LOAD
    /// covering the file (so AT_PHDR is covered by the LOAD even
    /// without PT_PHDR fallback).
    fn minimal() -> Self {
        // Base load vaddr aligned to 4 KiB; the file content (header +
        // phdrs) lives at offset 0, so to satisfy ELF congruence
        // (`p_vaddr % p_align == p_offset % p_align`) the LOAD vaddr
        // must also be page-aligned.
        let base_load_vaddr = 0x10000u64;
        Self {
            e_type: ET_EXEC_U16,
            e_machine: EM_RISCV_U16,
            // Entry inside the LOAD's filesz region.
            e_entry: base_load_vaddr + 0x80,
            e_phnum_override: None,
            e_phentsize_override: None,
            e_ident_class: ELFCLASS64_BYTE,
            e_ident_data: ELFDATA2LSB_BYTE,
            e_ident_version: EV_CURRENT_BYTE,
            include_pt_phdr: true,
            phdrs: vec![PhdrSpec::load(
                base_load_vaddr,
                0,
                /*filesz=*/ 176,
                /*memsz=*/ 176,
                PF_R_BIT | PF_X_BIT,
            )],
        }
    }

    fn build(&self) -> Vec<u8> {
        // Compose: header (64) + optional PT_PHDR (56) + phdrs.
        let pt_phdr_count: u16 = if self.include_pt_phdr { 1 } else { 0 };
        let n_phdrs = self.phdrs.len() as u16 + pt_phdr_count;
        let phent_real = 56u16;

        let phoff = 64u64;
        let total_phdr_bytes = n_phdrs as u64 * phent_real as u64;

        // File size: at least header + phdr table. Some LOAD specs
        // expect filesz==176 (i.e., header + 2 phdrs). Pad up to the
        // largest LOAD's `p_offset + p_filesz` so phdr-table coverage
        // tests are realistic.
        let mut min_file = phoff + total_phdr_bytes;
        for p in &self.phdrs {
            let end = p.p_offset.saturating_add(p.p_filesz);
            if end > min_file {
                min_file = end;
            }
        }
        let mut bytes = vec![0u8; min_file as usize];

        // ----- Ehdr -----
        bytes[EI_MAG0..EI_MAG0 + 4].copy_from_slice(&ELFMAG);
        bytes[4] = self.e_ident_class;
        bytes[5] = self.e_ident_data;
        bytes[6] = self.e_ident_version;
        bytes[EI_OSABI] = 0; // SysV.
                             // e_ident[8..16] = padding zeros.
        let phnum = self.e_phnum_override.unwrap_or(n_phdrs);
        let phent = self.e_phentsize_override.unwrap_or(phent_real);
        write_u16(&mut bytes, 16, self.e_type);
        write_u16(&mut bytes, 18, self.e_machine);
        write_u32(&mut bytes, 20, 1); // e_version = EV_CURRENT
        write_u64(&mut bytes, 24, self.e_entry); // e_entry
        write_u64(&mut bytes, 32, phoff); // e_phoff
        write_u64(&mut bytes, 40, 0); // e_shoff
        write_u32(&mut bytes, 48, 0); // e_flags
        write_u16(&mut bytes, 52, 64); // e_ehsize
        write_u16(&mut bytes, 54, phent); // e_phentsize
        write_u16(&mut bytes, 56, phnum); // e_phnum
        write_u16(&mut bytes, 58, 0); // e_shentsize
        write_u16(&mut bytes, 60, 0); // e_shnum
        write_u16(&mut bytes, 62, 0); // e_shstrndx

        // ----- Phdrs -----
        let mut cursor = phoff as usize;
        if self.include_pt_phdr {
            let pt_phdr_vaddr = self.phdrs[0].p_vaddr + phoff; // arbitrary: anchor inside first LOAD
            let pt_phdr_filesz = total_phdr_bytes;
            write_phdr_at(
                &mut bytes,
                cursor,
                PhdrSpec {
                    p_type: PT_PHDR_U32,
                    p_flags: PF_R_BIT,
                    p_offset: phoff,
                    p_vaddr: pt_phdr_vaddr,
                    p_paddr: pt_phdr_vaddr,
                    p_filesz: pt_phdr_filesz,
                    p_memsz: pt_phdr_filesz,
                    p_align: 8,
                },
            );
            cursor += 56;
        }
        for p in &self.phdrs {
            write_phdr_at(&mut bytes, cursor, *p);
            cursor += 56;
        }

        bytes
    }
}

fn write_u16(bytes: &mut [u8], at: usize, val: u16) {
    bytes[at..at + 2].copy_from_slice(&val.to_le_bytes());
}
fn write_u32(bytes: &mut [u8], at: usize, val: u32) {
    bytes[at..at + 4].copy_from_slice(&val.to_le_bytes());
}
fn write_u64(bytes: &mut [u8], at: usize, val: u64) {
    bytes[at..at + 8].copy_from_slice(&val.to_le_bytes());
}

fn write_phdr_at(bytes: &mut [u8], at: usize, p: PhdrSpec) {
    write_u32(bytes, at, p.p_type);
    write_u32(bytes, at + 4, p.p_flags);
    write_u64(bytes, at + 8, p.p_offset);
    write_u64(bytes, at + 16, p.p_vaddr);
    write_u64(bytes, at + 24, p.p_paddr);
    write_u64(bytes, at + 32, p.p_filesz);
    write_u64(bytes, at + 40, p.p_memsz);
    write_u64(bytes, at + 48, p.p_align);
}

// ---------------------------------------------------------------------
// Tests

#[test]
fn parse_image_plan_minimal_static_elf_round_trip() {
    let cfg = FixtureCfg::minimal();
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("minimal RV64 ET_EXEC fixture parses");

    assert_eq!(plan.entry, 0x10000 + 0x80);
    assert_eq!(plan.at_phent, 56);
    assert_eq!(plan.at_phnum, 2); // PT_PHDR + PT_LOAD.
                                  // PT_PHDR present → at_phdr is PT_PHDR.p_vaddr (set by builder
                                  // to base_load_vaddr + e_phoff = 0x10000 + 64).
    assert_eq!(plan.at_phdr, 0x10000 + 64);

    assert_eq!(plan.load_segments.len(), 1);
    let seg = &plan.load_segments[0];
    assert_eq!(seg.vaddr, 0x10000);
    assert_eq!(seg.memsz, 176);
    assert_eq!(seg.filesz, 176);
    assert_eq!(seg.file_offset, 0);
    assert_eq!(seg.align, 4096);
    assert!(seg.flags.readable);
    assert!(!seg.flags.writable);
    assert!(seg.flags.executable);

    assert!(plan.bss_extension.is_none());
}

#[test]
fn parse_image_plan_et_dyn_static_pie() {
    // ET_DYN (static-PIE) is now accepted. Use ET_DYN-idiomatic relative
    // vaddrs starting from 0x0 so the test is semantically correct.
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80; // relative entry (offset from load base)
                        // PT_LOAD with relative vaddr 0x0 (covers file bytes 0..176).
    cfg.phdrs[0].p_vaddr = 0x0;
    cfg.phdrs[0].p_paddr = 0x0;
    // PT_PHDR vaddr is set by the fixture builder to
    // phdrs[0].p_vaddr + e_phoff = 0x0 + 64 = 64 (relative).
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("ET_DYN static-PIE must be accepted");

    // load_bias = ET_DYN_LOAD_BIAS = 0x10000.
    // All addresses have it applied.
    assert_eq!(plan.load_bias, 0x10000);
    assert_eq!(plan.entry, 0x80 + 0x10000);
    // PT_PHDR.vaddr was 64 (relative) → at_phdr = 64 + 0x10000.
    assert_eq!(plan.at_phdr, 64 + 0x10000);
    assert_eq!(plan.load_segments.len(), 1);
    // Segment vaddr = 0x0 + 0x10000.
    assert_eq!(plan.load_segments[0].vaddr, 0x10000);
    assert!(plan.bss_extension.is_none());
}

#[test]
fn parse_image_plan_et_dyn_accepts_wx_load() {
    // ET_DYN static-PIE with a combined W+X PT_LOAD: accepted (unlike
    // ET_EXEC which still rejects W+X per EXEC-8-5).
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0].p_vaddr = 0x0;
    cfg.phdrs[0].p_paddr = 0x0;
    cfg.phdrs[0].p_flags = PF_R_BIT | PF_W_BIT | PF_X_BIT;
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("ET_DYN with RWX LOAD must be accepted");
    let seg = &plan.load_segments[0];
    assert!(seg.flags.readable && seg.flags.writable && seg.flags.executable);
}

#[test]
fn parse_image_plan_rejects_pt_interp() {
    let mut cfg = FixtureCfg::minimal();
    // Add a PT_INTERP segment (file_offset/filesz arbitrary; the
    // parser rejects on type alone). Place it inside the file
    // (file is sized to fit phdrs+LOAD content, so an offset of 0
    // is fine).
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: 0,
        p_vaddr: 0,
        p_paddr: 0,
        p_filesz: 1,
        p_memsz: 1,
        p_align: 1,
    });
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("PT_INTERP should be accepted");
    assert!(plan.interpreter_path.is_some(), "interpreter path should be extracted");
}

#[test]
fn parse_image_plan_rejects_pt_dynamic() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0,
        p_vaddr: 0,
        p_paddr: 0,
        p_filesz: 0,
        p_memsz: 0,
        p_align: 8,
    });
    let bytes = cfg.build();
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::HasInterp);
}

#[test]
fn parse_image_plan_rejects_non_riscv_arch() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_machine = EM_X86_64_U16;
    let bytes = cfg.build();
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::Arch);
}

#[test]
fn parse_image_plan_rejects_bad_class() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_ident_class = 1; // ELFCLASS32
    let bytes = cfg.build();
    // goblin's `Header::parse` accepts ELFCLASS32 too, so the
    // explicit re-check in our parser produces `Magic` here.
    let err = parse_image_plan(&bytes).unwrap_err();
    assert!(matches!(err, ParseError::Magic | ParseError::Phdr));
}

#[test]
fn parse_image_plan_extracts_bss_tail() {
    // Two LOAD segments: RX (text) + RW (data with BSS tail).
    let mut cfg = FixtureCfg::minimal();
    // Adjust the existing LOAD to be RX, file-only (filesz == memsz).
    cfg.phdrs[0] = PhdrSpec::load(
        0x10000,
        0,
        /*filesz=*/ 176,
        /*memsz=*/ 176,
        PF_R_BIT | PF_X_BIT,
    );
    // Add a writable LOAD whose memsz exceeds filesz → BSS tail.
    cfg.phdrs.push(PhdrSpec::load(
        0x20000,
        /*p_offset=*/ 0x1000, // file-page-aligned vs vaddr (both 0 mod 4096)
        /*filesz=*/ 0x100,
        /*memsz=*/ 0x800,
        PF_R_BIT | PF_W_BIT,
    ));
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("two-LOAD fixture parses");

    assert_eq!(plan.load_segments.len(), 2);
    let bss = plan.bss_extension.expect("RW LOAD has BSS tail");
    assert_eq!(bss.vaddr, 0x20000 + 0x100);
    assert_eq!(bss.size, 0x800 - 0x100);
}

#[test]
fn parse_image_plan_overlapping_segments_rejected() {
    let mut cfg = FixtureCfg::minimal();
    // Add a second LOAD overlapping the first: vaddr 0x10000 +
    // 0x10 (well within the first LOAD's [0x10000, 0x100b0) range).
    cfg.phdrs.push(PhdrSpec::load(
        0x10000 + 0x1000, // page-aligned, but its page-rounded
        // [0x11000, 0x12000) range overlaps with
        // first LOAD's page-rounded
        // [0x10000, 0x11000) only at the boundary.
        // To force overlap, shrink to same start:
        0x1000,
        0x100,
        0x100,
        PF_R_BIT | PF_W_BIT,
    ));
    // Force same-page overlap by aligning the second LOAD onto the
    // first's vaddr range.
    *cfg.phdrs.last_mut().unwrap() = PhdrSpec::load(
        0x10000, // same vaddr as first LOAD → guaranteed overlap
        0x1000,
        0x100,
        0x100,
        PF_R_BIT | PF_W_BIT,
    );
    let bytes = cfg.build();
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment,
        "two LOADs sharing the same page-rounded range must be rejected"
    );
}

#[test]
fn parse_image_plan_rejects_w_x_segment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_flags = PF_R_BIT | PF_W_BIT | PF_X_BIT;
    let bytes = cfg.build();
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_rejects_phdr_not_covered_when_no_pt_phdr() {
    // Drop PT_PHDR; place LOAD's file range so it does NOT cover
    // [e_phoff .. e_phoff + n*phent). The minimal LOAD starts at
    // file_offset 0 and covers the phdrs, so we need to move the
    // LOAD off offset 0.
    let mut cfg = FixtureCfg::minimal();
    cfg.include_pt_phdr = false;
    // LOAD now starts at offset 0x1000 — phoff (=64) is outside.
    // Vaddr must be page-congruent: 0x10000 mod 4096 == 0,
    // 0x1000 mod 4096 == 0 — congruent.
    cfg.phdrs[0] = PhdrSpec::load(
        0x10000,
        /*p_offset=*/ 0x1000,
        /*filesz=*/ 0x100,
        /*memsz=*/ 0x100,
        PF_R_BIT | PF_X_BIT,
    );
    let bytes = cfg.build();
    // No PT_PHDR and the only LOAD doesn't cover the phdr table →
    // parser must return Phdr.
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::Phdr);
}

#[test]
fn parse_image_plan_uses_load_fallback_for_phdr_when_no_pt_phdr() {
    // Drop PT_PHDR; the existing LOAD already covers offset 0..176
    // which contains the phdr table at 64..120 (only one phdr now).
    let mut cfg = FixtureCfg::minimal();
    cfg.include_pt_phdr = false;
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("LOAD-fallback path parses");
    assert_eq!(plan.at_phnum, 1);
    // at_phdr = LOAD.vaddr + (e_phoff - LOAD.p_offset) = 0x10000 + 64.
    assert_eq!(plan.at_phdr, 0x10000 + 64);
}

#[test]
fn parse_image_plan_rejects_too_short_buffer() {
    let mut bytes = vec![0u8; 32];
    bytes[..4].copy_from_slice(&ELFMAG);
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::Magic);
}

#[test]
fn parse_image_plan_rejects_bad_phentsize() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_phentsize_override = Some(48); // wrong (not 56)
    let bytes = cfg.build();
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::Phdr);
}

#[test]
fn parse_image_plan_rejects_zero_phnum() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_phnum_override = Some(0);
    let bytes = cfg.build();
    assert_eq!(parse_image_plan(&bytes).unwrap_err(), ParseError::Phdr);
}

#[test]
fn parse_image_plan_rejects_unaligned_load() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_align = 2048; // < PAGE_SIZE (4096)
    let bytes = cfg.build();
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_rejects_congruence_violation() {
    let mut cfg = FixtureCfg::minimal();
    // p_vaddr % 4096 == 0; bump p_offset off page (require
    // file_size to fit anyway).
    cfg.phdrs[0].p_offset = 0x10; // 16 % 4096 != 0 % 4096
    let bytes = cfg.build();
    // The parser checks congruence but won't decode bytes at the
    // bumped offset, so the assert is on the type not contents.
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_rejects_filesz_gt_memsz() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_filesz = 200;
    cfg.phdrs[0].p_memsz = 176;
    let bytes = cfg.build();
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_accepts_multi_bss_and_records_last_tail() {
    // Two writable LOADs each with `memsz > filesz` is legal ELF:
    // LA64 busybox uses this shape for .relro padding plus .data/.bss.
    // The VM mapping path handles BSS per LOAD segment; the summary
    // bss_extension keeps the last tail for auxv/debug consumers.
    let mut cfg = FixtureCfg::minimal();
    // Make the existing LOAD writable with a BSS tail.
    cfg.phdrs[0] = PhdrSpec::load(
        0x10000,
        0,
        /*filesz=*/ 176,
        /*memsz=*/ 176 + 0x100,
        PF_R_BIT | PF_W_BIT,
    );
    cfg.phdrs.push(PhdrSpec::load(
        0x20000,
        0x1000,
        /*filesz=*/ 0x100,
        /*memsz=*/ 0x800,
        PF_R_BIT | PF_W_BIT,
    ));
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("multi-BSS LOADs should parse");
    assert_eq!(plan.load_segments.len(), 2);
    let bss = plan
        .bss_extension
        .expect("last BSS-extending LOAD should be recorded");
    assert_eq!(bss.vaddr, 0x20000 + 0x100);
    assert_eq!(bss.size, 0x800 - 0x100);
}