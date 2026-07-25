//! Tests for the ELF parser binding (Phase 4).
//!
//! Fixtures are hand-crafted byte-by-byte minimal ELF64 ET_EXEC
//! images without requiring checked-in executable artifacts. Each test
//! mutates the base image to produce the
//! desired rejection. Layout reference: `Elf64_Ehdr` (64 bytes) +
//! `Elf64_Phdr` * N (56 bytes each), all little-endian.
//!
//! The fixtures verify the parser contract; round-trip parsing is
//! exercised by `parse_image_plan` itself through the production
//! `Elf08Parser` adapter.

use alloc::vec;
use alloc::vec::Vec;

use super::*;
use tx_hal::Arch;

struct FixtureParser;

impl ElfFileParser for FixtureParser {
    fn parse_header(_: &[u8]) -> Result<ElfHeader, ElfDecodeError> {
        Ok(ElfHeader::elf64_le(
            ET_EXEC, EM_RISCV, 0x10080, 64, 56, 2, 0,
        ))
    }

    fn parse_program_headers(
        _: &ElfHeader,
        _: &[u8],
    ) -> Result<Vec<ElfProgramHeader>, ElfDecodeError> {
        Ok(Vec::new())
    }
}

#[test]
fn parser_trait_exposes_only_tx_owned_values() {
    let header = FixtureParser::parse_header(&[]).unwrap();
    assert_eq!(header.entry, 0x10080);
}

#[test]
fn decoder_out_of_memory_remains_a_plan_out_of_memory_error() {
    assert_eq!(
        map_header_decode_error(ElfDecodeError::OutOfMemory),
        ParseError::OutOfMemory
    );
}

#[test]
fn elf08_parser_decodes_header_and_independent_phdr_table() {
    let mut cfg = FixtureCfg::minimal();
    cfg.include_pt_phdr = false;
    let mut bytes = cfg.build();

    write_u16(&mut bytes, 16, 0x0102);
    write_u16(&mut bytes, 18, 0x0304);
    write_u32(&mut bytes, 20, 0x0506_0708);
    write_u64(&mut bytes, 24, 0x1112_1314_1516_1718);
    write_u64(&mut bytes, 32, 0x2122_2324_2526_2728);
    write_u64(&mut bytes, 40, 0x3132_3334_3536_3738);
    write_u32(&mut bytes, 48, 0x4142_4344);
    write_u16(&mut bytes, 52, 0x5152);
    write_u16(&mut bytes, 54, 56);
    write_u16(&mut bytes, 56, 1);
    write_u16(&mut bytes, 58, 0x6162);
    write_u16(&mut bytes, 60, 0x7172);
    write_u16(&mut bytes, 62, 0x8182);

    let expected_phdr = PhdrSpec {
        p_type: 0x0102_0304,
        p_flags: 0x1112_1314,
        p_offset: 0x2122_2324_2526_2728,
        p_vaddr: 0x3132_3334_3536_3738,
        p_paddr: 0x4142_4344_4546_4748,
        p_filesz: 0x5152_5354_5556_5758,
        p_memsz: 0x6162_6364_6566_6768,
        p_align: 0x7172_7374_7576_7778,
    };
    write_phdr_at(&mut bytes, 64, expected_phdr);

    let header = Elf08Parser::parse_header(&bytes[..64]).unwrap();
    let table = &bytes[64..120];
    let phdrs = Elf08Parser::parse_program_headers(&header, table).unwrap();

    assert_eq!(header.class, ElfClass::Elf64);
    assert_eq!(header.endian, ElfEndian::Little);
    assert_eq!(header.elf_type, 0x0102);
    assert_eq!(header.machine, 0x0304);
    assert_eq!(header.version, 0x0506_0708);
    assert_eq!(header.entry, 0x1112_1314_1516_1718);
    assert_eq!(header.phoff, 0x2122_2324_2526_2728);
    assert_eq!(header.shoff, 0x3132_3334_3536_3738);
    assert_eq!(header.flags, 0x4142_4344);
    assert_eq!(header.ehsize, 0x5152);
    assert_eq!(header.phentsize, 56);
    assert_eq!(header.phnum, 1);
    assert_eq!(header.shentsize, 0x6162);
    assert_eq!(header.shnum, 0x7172);
    assert_eq!(header.shstrndx, 0x8182);

    assert_eq!(phdrs.len(), 1);
    assert_eq!(phdrs[0].p_type, expected_phdr.p_type);
    assert_eq!(phdrs[0].p_flags, expected_phdr.p_flags);
    assert_eq!(phdrs[0].p_offset, expected_phdr.p_offset);
    assert_eq!(phdrs[0].p_vaddr, expected_phdr.p_vaddr);
    assert_eq!(phdrs[0].p_paddr, expected_phdr.p_paddr);
    assert_eq!(phdrs[0].p_filesz, expected_phdr.p_filesz);
    assert_eq!(phdrs[0].p_memsz, expected_phdr.p_memsz);
    assert_eq!(phdrs[0].p_align, expected_phdr.p_align);
}

#[test]
fn elf08_parser_rejects_truncated_phdr_table() {
    let bytes = FixtureCfg::minimal().build();
    let header = Elf08Parser::parse_header(&bytes[..64]).unwrap();
    let table_len = header.phnum as usize * header.phentsize as usize;
    let table = &bytes[header.phoff as usize..header.phoff as usize + table_len - 1];

    assert_eq!(
        Elf08Parser::parse_program_headers(&header, table),
        Err(ElfDecodeError::Truncated)
    );
}

#[test]
fn elf08_parser_rejects_big_endian_header() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_ident_data = 2;
    let bytes = cfg.build();

    assert_eq!(
        Elf08Parser::parse_header(&bytes[..64]),
        Err(ElfDecodeError::UnsupportedEndian)
    );
}

#[test]
fn elf08_parser_classifies_truncated_header() {
    let bytes = FixtureCfg::minimal().build();

    assert_eq!(
        Elf08Parser::parse_header(&bytes[..15]),
        Err(ElfDecodeError::Truncated)
    );
    assert_eq!(
        Elf08Parser::parse_header(&bytes[..63]),
        Err(ElfDecodeError::Truncated)
    );
}

#[test]
fn elf08_parser_classifies_bad_magic() {
    let mut bytes = FixtureCfg::minimal().build();
    bytes[0] = 0;

    assert_eq!(
        Elf08Parser::parse_header(&bytes[..64]),
        Err(ElfDecodeError::BadMagic)
    );
}

#[test]
fn elf08_parser_classifies_unsupported_class() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_ident_class = 0;
    let bytes = cfg.build();

    assert_eq!(
        Elf08Parser::parse_header(&bytes[..64]),
        Err(ElfDecodeError::UnsupportedClass)
    );
}

#[test]
fn elf08_parser_classifies_unsupported_version() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_ident_version = 0;
    let bytes = cfg.build();

    assert_eq!(
        Elf08Parser::parse_header(&bytes[..64]),
        Err(ElfDecodeError::UnsupportedVersion)
    );
}

#[test]
fn elf08_parser_classifies_bad_program_header_entry_size() {
    let bytes = FixtureCfg::minimal().build();
    let mut header = Elf08Parser::parse_header(&bytes[..64]).unwrap();
    header.phentsize -= 1;

    assert_eq!(
        Elf08Parser::parse_program_headers(&header, &bytes[64..]),
        Err(ElfDecodeError::BadEntrySize)
    );
}

#[test]
fn elf08_parser_rejects_extra_program_header_bytes() {
    let bytes = FixtureCfg::minimal().build();
    let header = Elf08Parser::parse_header(&bytes[..64]).unwrap();
    let table_len = header.phnum as usize * header.phentsize as usize;
    let mut table = bytes[64..64 + table_len].to_vec();
    table.push(0);

    assert_eq!(
        Elf08Parser::parse_program_headers(&header, &table),
        Err(ElfDecodeError::Malformed)
    );
}

#[test]
fn elf08_parser_and_plan_builder_do_not_panic_on_bounded_bytes() {
    let base = FixtureCfg::minimal().build();
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);
    let mut state = 0x4d59_5df4_d0f3_3173u64;

    for case in 0..2048usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;

        let mut bytes = if case % 2 == 0 {
            let mut bytes = base.clone();
            let mutations = 1 + (state as usize % 4);
            for index in 0..mutations {
                state = state.rotate_left(9) ^ 0xa076_1d64_78bd_642f;
                let offset = state as usize % bytes.len();
                bytes[offset] ^= (state >> (index * 8)) as u8;
            }
            bytes.truncate(state as usize % (bytes.len() + 1));
            bytes
        } else {
            let len = state as usize % 513;
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                state = state.rotate_left(7) ^ 0xe703_7ed1_a0b4_28db;
                *byte = state as u8;
            }
            bytes
        };

        let outcome = std::panic::catch_unwind(|| {
            if let Ok(header) = Elf08Parser::parse_header(&bytes) {
                let _ = Elf08Parser::parse_program_headers(&header, &bytes);
            }
            let _ = parse_image_plan_with::<Elf08Parser>(&bytes, policy);
        });
        assert!(outcome.is_ok(), "bounded ELF corpus case {case} panicked");

        bytes.clear();
    }
}

// ELF identification offsets from the System V ABI.
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
const EM_LOONGARCH_U16: u16 = 258;
const EM_X86_64_U16: u16 = 62;

const EF_RISCV_RVC: u32 = 0x0001;
const EF_RISCV_FLOAT_ABI_DOUBLE: u32 = 0x0004;
const EF_RISCV_FLOAT_ABI_QUAD: u32 = 0x0006;
const EF_RISCV_RVE: u32 = 0x0008;
const EF_LARCH_ABI_DOUBLE_FLOAT: u32 = 0x03;
const EF_LARCH_OBJABI_V1: u32 = 0x40;

const PT_LOAD_U32: u32 = 1;
const PT_DYNAMIC_U32: u32 = 2;
const PT_INTERP_U32: u32 = 3;
const PT_PHDR_U32: u32 = 6;
const PT_TLS_U32: u32 = 7;
const PT_GNU_STACK_U32: u32 = 0x6474_e551;
const PT_GNU_RELRO_U32: u32 = 0x6474_e552;

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
    e_flags: u32,
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
            e_flags: 0,
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
        let phdr_table_end = phoff + total_phdr_bytes;
        let mut phdrs = self.phdrs.clone();

        // PT_PHDR promises that the table is part of the loaded image. Keep
        // the base fixture valid when tests append more program headers.
        if self.include_pt_phdr {
            let first_load = &mut phdrs[0];
            if first_load.p_type == PT_LOAD_U32
                && first_load.p_offset == 0
                && first_load.p_filesz >= phoff
                && first_load.p_filesz < phdr_table_end
            {
                first_load.p_filesz = phdr_table_end;
                first_load.p_memsz = first_load.p_memsz.max(phdr_table_end);
            }
        }

        // File size: at least header + phdr table. Some LOAD specs
        // expect filesz==176 (i.e., header + 2 phdrs). Pad up to the
        // largest LOAD's `p_offset + p_filesz` so phdr-table coverage
        // tests are realistic.
        let mut min_file = phoff + total_phdr_bytes;
        for p in &phdrs {
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
        write_u32(&mut bytes, 48, self.e_flags);
        write_u16(&mut bytes, 52, 64); // e_ehsize
        write_u16(&mut bytes, 54, phent); // e_phentsize
        write_u16(&mut bytes, 56, phnum); // e_phnum
        write_u16(&mut bytes, 58, 0); // e_shentsize
        write_u16(&mut bytes, 60, 0); // e_shnum
        write_u16(&mut bytes, 62, 0); // e_shstrndx

        // ----- Phdrs -----
        let mut cursor = phoff as usize;
        if self.include_pt_phdr {
            let pt_phdr_vaddr = phdrs[0].p_vaddr + phoff; // arbitrary: anchor inside first LOAD
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
        for p in &phdrs {
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
    // ET_DYN static-PIE with a combined W+X PT_LOAD follows the same
    // Linux-compatible acceptance policy as ET_EXEC.
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
fn parse_image_plan_accepts_pt_interp() {
    let mut cfg = FixtureCfg::minimal();
    let interp = b"/ld\0";
    let interp_offset = 0x200usize;
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: interp_offset as u64,
        p_vaddr: 0,
        p_paddr: 0,
        p_filesz: interp.len() as u64,
        p_memsz: interp.len() as u64,
        p_align: 1,
    });
    let mut bytes = cfg.build();
    bytes[interp_offset..interp_offset + interp.len()].copy_from_slice(interp);
    let plan = parse_image_plan(&bytes).expect("PT_INTERP should be accepted");
    assert_eq!(
        plan.interpreter_path.as_deref(),
        Some(&interp[..interp.len() - 1])
    );
}

#[test]
fn parse_image_plan_rejects_relative_pt_interp() {
    let mut cfg = FixtureCfg::minimal();
    let interp = b"ld.so\0";
    let interp_offset = 0x200usize;
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: interp_offset as u64,
        p_vaddr: 0,
        p_paddr: 0,
        p_filesz: interp.len() as u64,
        p_memsz: interp.len() as u64,
        p_align: 1,
    });
    let mut bytes = cfg.build();
    bytes[interp_offset..interp_offset + interp.len()].copy_from_slice(interp);

    assert!(matches!(parse_image_plan(&bytes), Err(ParseError::Phdr)));
}

#[test]
fn parse_image_plan_accepts_pt_interp_with_pt_dynamic() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_filesz = 0x400;
    cfg.phdrs[0].p_memsz = 0x400;
    let interp_offset = 0x200usize;
    let interp = b"/lib/ld-musl-riscv64-sf.so.1\0";
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: interp_offset as u64,
        p_vaddr: 0x10200,
        p_paddr: 0x10200,
        p_filesz: interp.len() as u64,
        p_memsz: interp.len() as u64,
        p_align: 1,
    });
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x300,
        p_vaddr: 0x10300,
        p_paddr: 0x10300,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });
    let mut bytes = cfg.build();
    if bytes.len() < interp_offset + interp.len() {
        bytes.resize(interp_offset + interp.len(), 0);
    }
    bytes[interp_offset..interp_offset + interp.len()].copy_from_slice(interp);
    let plan =
        parse_image_plan(&bytes).expect("PT_INTERP-owned PT_DYNAMIC should parse for interpreter");
    assert_eq!(
        plan.interpreter_path.as_deref(),
        Some(&interp[..interp.len() - 1])
    );
}

#[test]
fn parse_image_plan_et_dyn_accepts_pt_interp_with_pt_dynamic() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x400, 0x400, PF_R_BIT | PF_X_BIT);
    let interp_offset = 0x200usize;
    let interp = b"/lib/ld-musl-riscv64-sf.so.1\0";
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: interp_offset as u64,
        p_vaddr: 0x200,
        p_paddr: 0x200,
        p_filesz: interp.len() as u64,
        p_memsz: interp.len() as u64,
        p_align: 1,
    });
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x300,
        p_vaddr: 0x300,
        p_paddr: 0x300,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });
    let mut bytes = cfg.build();
    if bytes.len() < interp_offset + interp.len() {
        bytes.resize(interp_offset + interp.len(), 0);
    }
    bytes[interp_offset..interp_offset + interp.len()].copy_from_slice(interp);
    let plan = parse_image_plan(&bytes).expect("ET_DYN with PT_INTERP and PT_DYNAMIC should parse");
    assert_eq!(
        plan.interpreter_path.as_deref(),
        Some(&interp[..interp.len() - 1])
    );
    assert_eq!(plan.load_bias, 0x10000);
}

#[test]
fn parse_image_plan_et_dyn_accepts_pt_dynamic_without_interp() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x400, 0x400, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x200,
        p_vaddr: 0x200,
        p_paddr: 0x200,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes)
        .expect("ET_DYN static-PIE with PT_DYNAMIC but no PT_INTERP should parse");
    assert!(plan.interpreter_path.is_none());
}

#[test]
fn parse_image_plan_rejects_pt_dynamic_without_interp() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x200,
        p_vaddr: 0x10200,
        p_paddr: 0x10200,
        p_filesz: 16,
        p_memsz: 16,
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
fn parse_image_plan_accepts_loongarch64_arch() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_machine = EM_LOONGARCH_U16;
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("loongarch64 ELF should parse");
    assert_eq!(plan.entry, 0x10080);
}

#[test]
fn parse_image_plan_rejects_bad_class() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_ident_class = 1; // ELFCLASS32
    let bytes = cfg.build();
    // The syntax parser recognizes ELFCLASS32, so the explicit ELF64
    // policy check produces `Magic` here.
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
fn parse_image_plan_accepts_w_x_segment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_flags = PF_R_BIT | PF_W_BIT | PF_X_BIT;
    let bytes = cfg.build();
    let plan = parse_image_plan(&bytes).expect("Linux permits W+X LOAD segments");
    assert!(plan.load_segments[0].flags.writable);
    assert!(plan.load_segments[0].flags.executable);
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
fn parse_image_plan_accepts_phdr_table_when_load_covers_its_start_only() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0] = PhdrSpec::load(0x10000, 0, 80, 0x100, PF_R_BIT | PF_X_BIT);

    let plan = parse_image_plan(&cfg.build()).expect("Linux derives AT_PHDR from LOAD start");
    assert_eq!(plan.at_phdr, 0x10000 + 64);
}

#[test]
fn parse_image_plan_ignores_inconsistent_pt_phdr_declaration() {
    let mut bytes = FixtureCfg::minimal().build();
    write_u64(&mut bytes, 64 + 16, 0x10000 + 0x80);
    write_u64(&mut bytes, 64 + 32, 1);
    write_u64(&mut bytes, 64 + 40, 2);

    let plan = parse_image_plan(&bytes).expect("PT_PHDR is not an extra rejection source");
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
fn parse_image_plan_rejects_non_power_of_two_load_alignment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_align = 3;
    let bytes = cfg.build();
    assert_eq!(
        parse_image_plan(&bytes).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_with_accepts_zero_load_alignment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_align = 0;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("p_align=0 means no alignment requirement");
}

#[test]
fn parse_image_plan_with_accepts_one_load_alignment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_align = 1;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("p_align=1 means no alignment requirement");
}

#[test]
fn parse_image_plan_with_accepts_subpage_power_of_two_alignment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_align = 2048;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("sub-page power-of-two p_align is valid ELF");
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
        PF_R_BIT | PF_W_BIT | PF_X_BIT,
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

#[test]
fn parse_image_plan_with_elf08_accepts_existing_fixture() {
    let bytes = FixtureCfg::minimal().build();
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    let plan = parse_image_plan_with::<Elf08Parser>(&bytes, policy)
        .expect("Elf08Parser should build a plan from the existing fixture");

    assert_eq!(plan.entry, 0x10080);
    assert_eq!(plan.at_phdr, 0x10040);
    assert_eq!(plan.load_segments.len(), 1);
}

#[test]
fn parse_image_plan_with_rejects_non_current_header_version() {
    let mut bytes = FixtureCfg::minimal().build();
    write_u32(&mut bytes, 20, 2);
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&bytes, policy).unwrap_err(),
        ParseError::Magic
    );
}

#[test]
fn parse_image_plan_with_rejects_overflowing_file_range() {
    let mut bytes = FixtureCfg::minimal().build();
    let load_phdr = 64 + 56;
    write_u64(&mut bytes, load_phdr + 8, u64::MAX);
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&bytes, policy).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_with_rejects_entry_outside_executable_load() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_entry = 0x20000;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_with_ignores_invalid_pt_phdr() {
    let mut bytes = FixtureCfg::minimal().build();
    write_u64(&mut bytes, 64 + 16, 0x20000);
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    let plan = parse_image_plan_with::<Elf08Parser>(&bytes, policy)
        .expect("PT_PHDR declarations do not override the PT_LOAD-derived address");
    assert_eq!(plan.at_phdr, 0x10000 + 64);
}

#[test]
fn parse_image_plan_with_accepts_wx_load() {
    let mut cfg = FixtureCfg::minimal();
    cfg.phdrs[0].p_flags = PF_R_BIT | PF_W_BIT | PF_X_BIT;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    let plan = parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("Linux permits a LOAD segment to record W+X permissions");

    assert!(plan.load_segments[0].flags.writable);
    assert!(plan.load_segments[0].flags.executable);
}

#[test]
fn parse_image_plan_with_rejects_cross_isa_image() {
    let bytes = FixtureCfg::minimal().build();
    let policy = ElfLoadPolicy::fixture(Arch::LoongArch64, 0x4000_0000);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&bytes, policy).unwrap_err(),
        ParseError::Arch
    );
}

#[test]
fn parse_image_plan_with_accepts_supported_riscv_flags() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_flags = EF_RISCV_RVC | EF_RISCV_FLOAT_ABI_DOUBLE;
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);

    parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("RV64 RVC plus double-float ABI flags should be accepted");
}

#[test]
fn parse_image_plan_with_rejects_unsupported_riscv_flags() {
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000);
    for flags in [EF_RISCV_RVE, EF_RISCV_FLOAT_ABI_QUAD, 0x8000_0000] {
        let mut cfg = FixtureCfg::minimal();
        cfg.e_flags = flags;

        assert_eq!(
            parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy).unwrap_err(),
            ParseError::Arch,
            "unsupported RV64 e_flags {flags:#x} must be rejected"
        );
    }
}

#[test]
fn parse_image_plan_with_validates_loongarch_abi_flags() {
    let policy = ElfLoadPolicy::fixture(Arch::LoongArch64, 0x4000_0000);
    let mut cfg = FixtureCfg::minimal();
    cfg.e_machine = EM_LOONGARCH_U16;
    cfg.e_flags = EF_LARCH_OBJABI_V1 | EF_LARCH_ABI_DOUBLE_FLOAT;
    parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy)
        .expect("LoongArch double-float object ABI v1 should be accepted");

    for flags in [0x04, 0x80, EF_LARCH_OBJABI_V1 | 0x07] {
        cfg.e_flags = flags;
        assert_eq!(
            parse_image_plan_with::<Elf08Parser>(&cfg.build(), policy).unwrap_err(),
            ParseError::Arch,
            "unsupported LoongArch e_flags {flags:#x} must be rejected"
        );
    }
}

#[test]
fn parse_image_plan_with_rejects_final_range_at_user_top() {
    let bytes = FixtureCfg::minimal().build();
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x10080);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&bytes, policy).unwrap_err(),
        ParseError::LoadSegment
    );
}

#[test]
fn parse_image_plan_with_rejects_phdr_table_end_past_user_top() {
    let mut cfg = FixtureCfg::minimal();
    cfg.include_pt_phdr = false;
    cfg.e_entry = 0x10010;
    let mut bytes = cfg.build();
    let phoff = 0xff0usize;
    bytes.resize(phoff + ELF64_PHENT as usize, 0);
    write_u64(&mut bytes, 32, phoff as u64);
    write_phdr_at(
        &mut bytes,
        phoff,
        PhdrSpec::load(0x10000, 0, 0xff1, 0xff1, PF_R_BIT | PF_X_BIT),
    );
    let policy = ElfLoadPolicy::fixture(Arch::Riscv64, 0x11000);

    assert_eq!(
        parse_image_plan_with::<Elf08Parser>(&bytes, policy).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_tls_relro_dynamic_and_gnu_stack() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x3000, 0x3000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.extend([
        PhdrSpec {
            p_type: PT_TLS_U32,
            p_flags: PF_R_BIT,
            p_offset: 0x200,
            p_vaddr: 0x200,
            p_paddr: 0x200,
            p_filesz: 16,
            p_memsz: 32,
            p_align: 16,
        },
        PhdrSpec {
            p_type: PT_GNU_RELRO_U32,
            p_flags: PF_R_BIT,
            p_offset: 0x1000,
            p_vaddr: 0x1000,
            p_paddr: 0x1000,
            p_filesz: 4096,
            p_memsz: 4096,
            p_align: 1,
        },
        PhdrSpec {
            p_type: PT_DYNAMIC_U32,
            p_flags: PF_R_BIT,
            p_offset: 0x300,
            p_vaddr: 0x300,
            p_paddr: 0x300,
            p_filesz: 16,
            p_memsz: 16,
            p_align: 8,
        },
        PhdrSpec {
            p_type: PT_GNU_STACK_U32,
            p_flags: PF_R_BIT | PF_W_BIT | PF_X_BIT,
            p_offset: 0,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: 0,
            p_memsz: 0,
            p_align: 16,
        },
    ]);

    let plan = parse_image_plan(&cfg.build()).expect("runtime metadata should be retained");

    let tls = plan.tls.expect("PT_TLS");
    assert_eq!(tls.vaddr, ET_DYN_LOAD_BIAS + 0x200);
    assert_eq!(tls.file_offset, 0x200);
    assert_eq!(tls.file_size, 16);
    assert_eq!(tls.memory_size, 32);
    assert_eq!(tls.align, 16);
    assert_eq!(
        plan.relro,
        Some(ImageRange {
            vaddr: ET_DYN_LOAD_BIAS + 0x1000,
            size: 4096,
        })
    );
    assert_eq!(
        plan.dynamic,
        Some(ImageRange {
            vaddr: ET_DYN_LOAD_BIAS + 0x300,
            size: 16,
        })
    );
    assert!(plan.stack.executable_requested);
}

#[test]
fn parse_image_plan_records_missing_gnu_stack_as_nx_request() {
    let plan = parse_image_plan(&FixtureCfg::minimal().build()).unwrap();

    assert_eq!(
        plan.stack,
        StackRequest {
            executable_requested: false,
        }
    );
}

#[test]
fn parse_image_plan_records_rejects_duplicate_dynamic_and_tls() {
    for p_type in [PT_TLS_U32, PT_DYNAMIC_U32] {
        let mut cfg = FixtureCfg::minimal();
        cfg.e_type = ET_DYN_U16;
        cfg.e_entry = 0x80;
        cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
        let metadata = PhdrSpec {
            p_type,
            p_flags: PF_R_BIT,
            p_offset: 0x200,
            p_vaddr: 0x200,
            p_paddr: 0x200,
            p_filesz: 16,
            p_memsz: 16,
            p_align: 8,
        };
        cfg.phdrs.extend([metadata, metadata]);

        assert_eq!(
            parse_image_plan(&cfg.build()).unwrap_err(),
            ParseError::Phdr,
            "duplicate program-header type {p_type:#x} must not depend on order"
        );
    }
}

#[test]
fn parse_image_plan_records_rejects_duplicate_interp() {
    let mut cfg = FixtureCfg::minimal();
    let interp = PhdrSpec {
        p_type: PT_INTERP_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x200,
        p_vaddr: 0x10200,
        p_paddr: 0x10200,
        p_filesz: 1,
        p_memsz: 1,
        p_align: 1,
    };
    cfg.phdrs.extend([interp, interp]);

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::HasInterp
    );
}

#[test]
fn parse_image_plan_records_rejects_metadata_file_ranges_outside_image() {
    for p_type in [PT_TLS_U32, PT_DYNAMIC_U32, PT_GNU_RELRO_U32] {
        let mut cfg = FixtureCfg::minimal();
        cfg.e_type = ET_DYN_U16;
        cfg.e_entry = 0x80;
        cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
        cfg.phdrs.push(PhdrSpec {
            p_type,
            p_flags: PF_R_BIT,
            p_offset: 0,
            p_vaddr: 0x200,
            p_paddr: 0x200,
            p_filesz: 16,
            p_memsz: 16,
            p_align: 8,
        });
        let mut bytes = cfg.build();
        let metadata_phdr = 64 + 2 * ELF64_PHENT as usize;
        let outside = bytes.len() as u64;
        write_u64(&mut bytes, metadata_phdr + 8, outside);

        assert_eq!(
            parse_image_plan(&bytes).unwrap_err(),
            ParseError::Phdr,
            "program-header type {p_type:#x} must have a bounded file range"
        );
    }
}

#[test]
fn parse_image_plan_records_rejects_metadata_file_and_va_overflow() {
    for (field_offset, value, p_types) in [
        (
            8usize,
            u64::MAX,
            &[PT_TLS_U32, PT_DYNAMIC_U32, PT_GNU_RELRO_U32][..],
        ),
        (
            16usize,
            u64::MAX,
            &[PT_TLS_U32, PT_DYNAMIC_U32, PT_GNU_RELRO_U32][..],
        ),
        (
            16usize,
            u64::MAX - ET_DYN_LOAD_BIAS + 1,
            &[PT_TLS_U32, PT_DYNAMIC_U32, PT_GNU_RELRO_U32][..],
        ),
    ] {
        for &p_type in p_types {
            let mut cfg = FixtureCfg::minimal();
            cfg.e_type = ET_DYN_U16;
            cfg.e_entry = 0x80;
            cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
            cfg.phdrs.push(PhdrSpec {
                p_type,
                p_flags: PF_R_BIT,
                p_offset: 0x200,
                p_vaddr: 0x200,
                p_paddr: 0x200,
                p_filesz: 16,
                p_memsz: 16,
                p_align: 8,
            });
            let mut bytes = cfg.build();
            let metadata_phdr = 64 + 2 * ELF64_PHENT as usize;
            write_u64(&mut bytes, metadata_phdr + field_offset, value);

            assert_eq!(
                parse_image_plan(&bytes).unwrap_err(),
                ParseError::Phdr,
                "program-header type {p_type:#x} must reject range overflow"
            );
        }
    }
}

#[test]
fn parse_image_plan_records_rejects_tls_file_size_larger_than_memory_size() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_TLS_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x200,
        p_vaddr: 0x200,
        p_paddr: 0x200,
        p_filesz: 32,
        p_memsz: 16,
        p_align: 8,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_dynamic_and_tls_across_contiguous_loads() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec::load(
        0x1000,
        0x1000,
        0x1000,
        0x1000,
        PF_R_BIT | PF_W_BIT,
    ));
    cfg.phdrs.extend([
        PhdrSpec {
            p_type: PT_DYNAMIC_U32,
            p_flags: PF_R_BIT,
            p_offset: 0xff0,
            p_vaddr: 0xff0,
            p_paddr: 0xff0,
            p_filesz: 0x20,
            p_memsz: 0x20,
            p_align: 8,
        },
        PhdrSpec {
            p_type: PT_TLS_U32,
            p_flags: PF_R_BIT,
            p_offset: 0xfe0,
            p_vaddr: 0xfe0,
            p_paddr: 0xfe0,
            p_filesz: 0x20,
            p_memsz: 0x40,
            p_align: 16,
        },
    ]);

    let plan = parse_image_plan(&cfg.build()).expect("contiguous LOAD union covers metadata");

    assert_eq!(plan.dynamic.unwrap().vaddr, ET_DYN_LOAD_BIAS + 0xff0);
    assert_eq!(plan.tls.unwrap().vaddr, ET_DYN_LOAD_BIAS + 0xfe0);
}

#[test]
fn parse_image_plan_records_rejects_dynamic_outside_load_coverage() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x2000,
        p_vaddr: 0x2000,
        p_paddr: 0x2000,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_rejects_dynamic_crossing_a_load_union_gap() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec::load(
        0x2000,
        0x2000,
        0x1000,
        0x1000,
        PF_R_BIT | PF_W_BIT,
    ));
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        p_offset: 0xff0,
        p_vaddr: 0xff0,
        p_paddr: 0xff0,
        p_filesz: 0x1020,
        p_memsz: 0x1020,
        p_align: 8,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_rejects_dynamic_with_mismatched_load_mapping_delta() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec::load(
        0x3000,
        0x1000,
        0x1000,
        0x1000,
        PF_R_BIT | PF_W_BIT,
    ));
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_DYNAMIC_U32,
        p_flags: PF_R_BIT,
        // The file bytes are in LOAD A while the VA is in LOAD B. Both
        // independent ranges are covered, but no LOAD maps this pair.
        p_offset: 0x800,
        p_vaddr: 0x3800,
        p_paddr: 0x3800,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_rejects_tls_template_outside_file_load_coverage() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x3000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_TLS_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x2000,
        p_vaddr: 0x200,
        p_paddr: 0x200,
        p_filesz: 16,
        p_memsz: 16,
        p_align: 8,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_relro_inside_bss_without_file_coverage() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x3000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_GNU_RELRO_U32,
        p_flags: PF_R_BIT,
        p_offset: 0x1800,
        p_vaddr: 0x1800,
        p_paddr: 0x1800,
        p_filesz: 0x400,
        p_memsz: 0x400,
        p_align: 1,
    });

    let plan = parse_image_plan(&cfg.build()).expect("RELRO may cover a LOAD's BSS bytes");

    assert_eq!(
        plan.relro,
        Some(ImageRange {
            vaddr: ET_DYN_LOAD_BIAS + 0x1800,
            size: 0x400,
        })
    );
}

#[test]
fn parse_image_plan_records_rejects_relro_outside_memory_load_coverage() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.push(PhdrSpec {
        p_type: PT_GNU_RELRO_U32,
        p_flags: PF_R_BIT,
        p_offset: 0,
        p_vaddr: 0x2000,
        p_paddr: 0x2000,
        p_filesz: 0,
        p_memsz: 0x100,
        p_align: 1,
    });

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn parse_image_plan_records_merges_overlapping_and_adjacent_relro_headers() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x3000, 0x3000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs.extend([
        PhdrSpec {
            p_type: PT_GNU_RELRO_U32,
            p_flags: PF_R_BIT,
            p_offset: 0,
            p_vaddr: 0x800,
            p_paddr: 0x800,
            p_filesz: 0,
            p_memsz: 0x800,
            p_align: 1,
        },
        PhdrSpec {
            p_type: PT_GNU_RELRO_U32,
            p_flags: PF_R_BIT,
            p_offset: 0,
            p_vaddr: 0xc00,
            p_paddr: 0xc00,
            p_filesz: 0,
            p_memsz: 0xc00,
            p_align: 1,
        },
        PhdrSpec {
            p_type: PT_GNU_RELRO_U32,
            p_flags: PF_R_BIT,
            p_offset: 0,
            p_vaddr: 0x1800,
            p_paddr: 0x1800,
            p_filesz: 0,
            p_memsz: 0x800,
            p_align: 1,
        },
    ]);

    let plan = parse_image_plan(&cfg.build()).expect("RELRO ranges form one covered union");

    assert_eq!(
        plan.relro,
        Some(ImageRange {
            vaddr: ET_DYN_LOAD_BIAS + 0x800,
            size: 0x1800,
        })
    );
}

#[test]
fn parse_image_plan_records_rejects_disjoint_relro_headers() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x3000, 0x3000, PF_R_BIT | PF_X_BIT);
    for vaddr in [0x800, 0x1800] {
        cfg.phdrs.push(PhdrSpec {
            p_type: PT_GNU_RELRO_U32,
            p_flags: PF_R_BIT,
            p_offset: 0,
            p_vaddr: vaddr,
            p_paddr: vaddr,
            p_filesz: 0,
            p_memsz: 0x100,
            p_align: 1,
        });
    }

    assert_eq!(
        parse_image_plan(&cfg.build()).unwrap_err(),
        ParseError::Phdr
    );
}

#[test]
fn exec_image_plan_checked_rebase_updates_every_address_atomically() {
    let plan = ExecImagePlan {
        entry: 0x1_0080,
        at_phdr: 0x1_0040,
        at_phent: ELF64_PHENT,
        at_phnum: 3,
        load_segments: vec![LoadSegment {
            vaddr: 0x1_0000,
            memsz: 0x3000,
            filesz: 0x2000,
            file_offset: 0,
            flags: SegmentFlags {
                readable: true,
                writable: false,
                executable: true,
            },
            align: 0x2000,
        }],
        bss_extension: Some(BssTail {
            vaddr: 0x1_2000,
            size: 0x1000,
        }),
        load_bias: 0x1_0000,
        tls: Some(TlsTemplate {
            vaddr: 0x1_1800,
            file_offset: 0x1800,
            file_size: 0x80,
            memory_size: 0x100,
            align: 0x20,
        }),
        dynamic: Some(ImageRange {
            vaddr: 0x1_1000,
            size: 0x100,
        }),
        relro: Some(ImageRange {
            vaddr: 0x1_2000,
            size: 0x800,
        }),
        stack: StackRequest {
            executable_requested: false,
        },
        interpreter_path: None,
    };

    let rebased = plan
        .clone()
        .checked_rebase(0x4_0000, 0x10_0000)
        .expect("checked rebase");
    assert_eq!(rebased.load_bias, 0x4_0000);
    assert_eq!(rebased.entry, 0x4_0080);
    assert_eq!(rebased.at_phdr, 0x4_0040);
    assert_eq!(rebased.load_segments[0].vaddr, 0x4_0000);
    assert_eq!(rebased.bss_extension.unwrap().vaddr, 0x4_2000);
    assert_eq!(rebased.tls.unwrap().vaddr, 0x4_1800);
    assert_eq!(rebased.dynamic.unwrap().vaddr, 0x4_1000);
    assert_eq!(rebased.relro.unwrap().vaddr, 0x4_2000);

    assert_eq!(
        plan.clone().checked_rebase(0x4_1000, 0x10_0000),
        Err(ElfLayoutError::InvalidAlignment)
    );

    assert_eq!(
        plan.checked_rebase(u64::MAX - 0x1fff, u64::MAX),
        Err(ElfLayoutError::AddressOverflow),
        "a failed rebase must not expose a partially adjusted plan"
    );
}

#[test]
fn parse_image_plan_records_gnu_stack_last_header_wins_and_ignores_ranges() {
    let mut cfg = FixtureCfg::minimal();
    for p_flags in [PF_R_BIT | PF_W_BIT | PF_X_BIT, PF_R_BIT | PF_W_BIT] {
        cfg.phdrs.push(PhdrSpec {
            p_type: PT_GNU_STACK_U32,
            p_flags,
            p_offset: 0,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: 0,
            p_memsz: 0,
            p_align: 1,
        });
    }
    let mut bytes = cfg.build();
    for phdr_at in [64 + 2 * ELF64_PHENT as usize, 64 + 3 * ELF64_PHENT as usize] {
        write_u64(&mut bytes, phdr_at + 8, u64::MAX);
        write_u64(&mut bytes, phdr_at + 16, u64::MAX);
        write_u64(&mut bytes, phdr_at + 24, u64::MAX);
        write_u64(&mut bytes, phdr_at + 32, u64::MAX);
        write_u64(&mut bytes, phdr_at + 40, u64::MAX);
        write_u64(&mut bytes, phdr_at + 48, 3);
    }

    let plan = parse_image_plan(&bytes).expect("GNU_STACK consumes only PF_X");

    assert!(!plan.stack.executable_requested);
}

#[test]
fn parse_image_plan_records_gnu_stack_last_executable_header_wins() {
    let mut cfg = FixtureCfg::minimal();
    for p_flags in [PF_R_BIT | PF_W_BIT, PF_R_BIT | PF_W_BIT | PF_X_BIT] {
        cfg.phdrs.push(PhdrSpec {
            p_type: PT_GNU_STACK_U32,
            p_flags,
            p_offset: 0,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: 0,
            p_memsz: 0,
            p_align: 16,
        });
    }

    assert!(
        parse_image_plan(&cfg.build())
            .unwrap()
            .stack
            .executable_requested
    );
}

#[test]
fn parse_image_plan_records_et_dyn_bias_honors_large_load_alignment() {
    let mut cfg = FixtureCfg::minimal();
    cfg.e_type = ET_DYN_U16;
    cfg.e_entry = 0x80;
    cfg.phdrs[0] = PhdrSpec::load(0, 0, 0x1000, 0x1000, PF_R_BIT | PF_X_BIT);
    cfg.phdrs[0].p_align = 0x20_000;

    let plan = parse_image_plan(&cfg.build()).expect("large aligned PIE should parse");

    assert_eq!(plan.load_bias % 0x20_000, 0);
    assert_eq!(plan.load_segments[0].vaddr % 0x20_000, 0);
}
