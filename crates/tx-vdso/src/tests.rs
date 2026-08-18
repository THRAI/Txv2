use super::{VDSO_AVAILABLE, VDSO_IMAGE, VDSO_RT_SIGRETURN_OFFSET, VVAR_DELTA};
use std::borrow::ToOwned;
use std::vec::Vec;
use tx_time::vdso::{VdsoClockMode, VVAR_CLOCK_MODE_OFFSET, VVAR_MULT_OFFSET};

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const SHT_PROGBITS: u32 = 1;

const DT_NULL: i64 = 0;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_REL: i64 = 17;
const DT_RELSZ: i64 = 18;
const DT_PLTREL: i64 = 20;
const DT_JMPREL: i64 = 23;
const DT_VERSYM: i64 = 0x6fff_fff0;
const DT_VERDEF: i64 = 0x6fff_fffc;
const DT_VERDEFNUM: i64 = 0x6fff_fffd;

const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: usize = 56;
const ELF64_DYN_SIZE: usize = 16;
const ELF64_SYM_SIZE: usize = 24;

#[derive(Clone, Copy)]
struct LoadSegment {
    offset: usize,
    vaddr: u64,
    filesz: usize,
}

fn le16(image: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(image[offset..offset + 2].try_into().unwrap())
}

fn le32(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap())
}

fn le64(image: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(image[offset..offset + 8].try_into().unwrap())
}

fn dynamic_entries(image: &[u8]) -> (Vec<LoadSegment>, Vec<(i64, u64)>) {
    assert!(
        VDSO_AVAILABLE,
        "host contract tests require a generated RV64 vDSO"
    );
    assert!(image.len() >= ELF64_EHDR_SIZE, "vDSO ELF header is missing");
    assert_eq!(&image[..4], b"\x7fELF");
    assert_eq!(image[4], 2, "ELFCLASS64");
    assert_eq!(image[5], 1, "little-endian ELF");
    assert_eq!(le16(image, 16), 3, "ET_DYN");
    assert_eq!(le16(image, 18), 243, "EM_RISCV");

    let phoff = le64(image, 32) as usize;
    let phentsize = le16(image, 54) as usize;
    let phnum = le16(image, 56) as usize;
    assert_eq!(phentsize, ELF64_PHDR_SIZE);

    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        let typ = le32(image, offset);
        let segment = LoadSegment {
            offset: le64(image, offset + 8) as usize,
            vaddr: le64(image, offset + 16),
            filesz: le64(image, offset + 32) as usize,
        };
        match typ {
            PT_LOAD => loads.push(segment),
            PT_DYNAMIC => dynamic = Some(segment),
            _ => {}
        }
    }

    assert!(!loads.is_empty(), "missing PT_LOAD");
    let dynamic = dynamic.expect("missing PT_DYNAMIC");
    assert_eq!(dynamic.filesz % ELF64_DYN_SIZE, 0);

    let mut entries = Vec::new();
    for offset in (dynamic.offset..dynamic.offset + dynamic.filesz).step_by(ELF64_DYN_SIZE) {
        let tag = le64(image, offset) as i64;
        entries.push((tag, le64(image, offset + 8)));
        if tag == DT_NULL {
            break;
        }
    }
    assert_eq!(entries.last().map(|entry| entry.0), Some(DT_NULL));
    (loads, entries)
}

fn dynamic_value(entries: &[(i64, u64)], tag: i64) -> u64 {
    entries
        .iter()
        .find_map(|(candidate, value)| (*candidate == tag).then_some(*value))
        .unwrap_or_else(|| panic!("missing dynamic tag {tag:#x}"))
}

fn vaddr_to_offset(loads: &[LoadSegment], address: u64) -> usize {
    loads
        .iter()
        .find_map(|segment| {
            (address >= segment.vaddr && address < segment.vaddr + segment.filesz as u64)
                .then_some(segment.offset + (address - segment.vaddr) as usize)
        })
        .unwrap_or_else(|| panic!("address {address:#x} is outside PT_LOAD"))
}

fn c_string(bytes: &[u8], offset: usize) -> &str {
    let tail = &bytes[offset..];
    let end = tail.iter().position(|byte| *byte == 0).unwrap();
    core::str::from_utf8(&tail[..end]).unwrap()
}

fn exported_symbol_value(name: &str) -> u64 {
    let (loads, entries) = dynamic_entries(VDSO_IMAGE);
    let dynstr = vaddr_to_offset(&loads, dynamic_value(&entries, DT_STRTAB));
    let dynsym = vaddr_to_offset(&loads, dynamic_value(&entries, DT_SYMTAB));
    let hash = vaddr_to_offset(&loads, dynamic_value(&entries, DT_HASH));
    let symbol_count = le32(VDSO_IMAGE, hash + 4) as usize;

    (1..symbol_count)
        .find_map(|index| {
            let symbol = dynsym + index * ELF64_SYM_SIZE;
            (c_string(VDSO_IMAGE, dynstr + le32(VDSO_IMAGE, symbol) as usize) == name)
                .then_some(le64(VDSO_IMAGE, symbol + 8))
        })
        .unwrap_or_else(|| panic!("missing exported symbol {name}"))
}

fn emitted_vvar_pc_delta(clock_gettime_vaddr: u64) -> (u64, isize) {
    let (loads, _) = dynamic_entries(VDSO_IMAGE);
    let clock_gettime = vaddr_to_offset(&loads, clock_gettime_vaddr);
    let auipc = (clock_gettime..clock_gettime + 64)
        .step_by(4)
        .find(|offset| le32(VDSO_IMAGE, *offset) & 0x7f == 0x17)
        .expect("clock_gettime VVAR base uses auipc");
    let lui = le32(VDSO_IMAGE, auipc + 4) as i32;
    let addiw = le32(VDSO_IMAGE, auipc + 8) as i32;

    assert_eq!(lui & 0x7f, 0x37, "auipc is followed by li's lui");
    assert_eq!(addiw & 0x7f, 0x1b, "auipc is followed by li's addiw");
    assert_eq!((lui >> 7) & 0x1f, 5, "li uses t0");
    assert_eq!((addiw >> 7) & 0x1f, 5, "li result stays in t0");
    assert_eq!((addiw >> 15) & 0x1f, 5, "addiw reads t0");

    let delta = (lui & !0xfff).wrapping_add(addiw >> 20) as isize;
    let auipc_vaddr = clock_gettime_vaddr + (auipc - clock_gettime) as u64;
    (auipc_vaddr, delta)
}

fn elf_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for byte in name {
        hash = hash.wrapping_shl(4).wrapping_add(u32::from(*byte));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

#[test]
fn generated_image_has_loader_required_dynamic_tags() {
    let (_, entries) = dynamic_entries(VDSO_IMAGE);
    assert!(dynamic_value(&entries, DT_STRTAB) > 0);
    assert!(dynamic_value(&entries, DT_SYMTAB) > 0);
    assert!(dynamic_value(&entries, DT_HASH) > 0);
    assert!(dynamic_value(&entries, DT_STRSZ) > 1);
    assert_eq!(dynamic_value(&entries, DT_SYMENT), ELF64_SYM_SIZE as u64);
}

#[test]
fn generated_image_exports_linux_4_15_time_symbols() {
    let (loads, entries) = dynamic_entries(VDSO_IMAGE);
    let dynstr = vaddr_to_offset(&loads, dynamic_value(&entries, DT_STRTAB));
    let dynsym = vaddr_to_offset(&loads, dynamic_value(&entries, DT_SYMTAB));
    let hash = vaddr_to_offset(&loads, dynamic_value(&entries, DT_HASH));
    let symbol_count = le32(VDSO_IMAGE, hash + 4) as usize;
    assert_eq!(symbol_count, 5, "SysV hash nchain includes STN_UNDEF");

    let names = (1..symbol_count)
        .map(|index| {
            let symbol = dynsym + index * ELF64_SYM_SIZE;
            assert_eq!(
                VDSO_IMAGE[symbol + 4],
                0x12,
                "symbol {index} is global function"
            );
            assert_ne!(le16(VDSO_IMAGE, symbol + 6), 0, "symbol {index} is defined");
            c_string(VDSO_IMAGE, dynstr + le32(VDSO_IMAGE, symbol) as usize).to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "__vdso_clock_gettime",
            "__vdso_gettimeofday",
            "__vdso_clock_getres",
            "__vdso_rt_sigreturn",
        ]
    );
    assert_eq!(
        VDSO_RT_SIGRETURN_OFFSET as u64,
        exported_symbol_value("__vdso_rt_sigreturn"),
        "the generated restorer offset must address the exported ELF symbol"
    );

    let versym = vaddr_to_offset(&loads, dynamic_value(&entries, DT_VERSYM));
    assert_eq!(le16(VDSO_IMAGE, versym), 0);
    for index in 1..symbol_count {
        assert_eq!(le16(VDSO_IMAGE, versym + index * 2), 2);
    }

    assert_eq!(dynamic_value(&entries, DT_VERDEFNUM), 1);
    let verdef = vaddr_to_offset(&loads, dynamic_value(&entries, DT_VERDEF));
    assert_eq!(le16(VDSO_IMAGE, verdef), 1, "Elf64_Verdef version");
    assert_eq!(le16(VDSO_IMAGE, verdef + 4), 2, "LINUX_4.15 version index");
    assert_eq!(le16(VDSO_IMAGE, verdef + 6), 1, "one Verdaux record");
    assert_eq!(le32(VDSO_IMAGE, verdef + 8), elf_hash(b"LINUX_4.15"));
    let aux = verdef + le32(VDSO_IMAGE, verdef + 12) as usize;
    assert_eq!(
        c_string(VDSO_IMAGE, dynstr + le32(VDSO_IMAGE, aux) as usize),
        "LINUX_4.15"
    );
}

#[test]
fn generated_image_has_no_dynamic_relocations() {
    let (_, entries) = dynamic_entries(VDSO_IMAGE);
    for tag in [DT_RELA, DT_RELASZ, DT_REL, DT_RELSZ, DT_PLTREL, DT_JMPREL] {
        assert!(
            entries.iter().all(|(candidate, _)| *candidate != tag),
            "relocation dynamic tag {tag:#x} must be absent"
        );
    }
}

#[test]
fn vvar_address_is_an_image_relative_delta() {
    let assembly = include_str!("vdso.S").to_ascii_lowercase();
    let build_script = include_str!("../build.rs");
    assert_eq!(VVAR_DELTA, -4096);
    assert!(!assembly.contains("0x3f_fffe_f000"));
    assert!(assembly.contains("vvar_pc_delta"));
    assert!(assembly.contains("auipc"));
    assert!(build_script.contains("VVAR_PC_DELTA"));
    assert!(build_script.contains("VVAR_DELTA"));
}

#[test]
fn emitted_vvar_delta_resolves_to_the_runtime_vvar_page() {
    let clock_gettime_vaddr = exported_symbol_value("__vdso_clock_gettime");
    let (auipc_vaddr, vvar_pc_delta) = emitted_vvar_pc_delta(clock_gettime_vaddr);
    let runtime_vdso_base = VDSO_IMAGE
        .len()
        .checked_mul(3)
        .expect("test runtime vDSO base fits");
    let runtime_auipc = runtime_vdso_base
        .checked_add(auipc_vaddr as usize)
        .expect("runtime auipc fits");
    let runtime_vvar = runtime_auipc
        .checked_add_signed(vvar_pc_delta)
        .expect("runtime VVAR address fits");
    let mapped_vvar = runtime_vdso_base
        .checked_add_signed(VVAR_DELTA)
        .expect("mapped VVAR address fits");

    assert_eq!(runtime_vvar, mapped_vvar);
}

#[test]
fn exported_entry_points_are_rv64_instruction_aligned() {
    for symbol in [
        "__vdso_clock_gettime",
        "__vdso_gettimeofday",
        "__vdso_clock_getres",
        "__vdso_rt_sigreturn",
    ] {
        assert_eq!(
            exported_symbol_value(symbol) % 4,
            0,
            "{symbol} must be a four-byte aligned RV64 entry point"
        );
    }
}

#[test]
fn exported_clock_gettime_starts_at_the_final_text_section() {
    let section_table = le64(VDSO_IMAGE, 40) as usize;
    let section_size = le16(VDSO_IMAGE, 58) as usize;
    let section_count = le16(VDSO_IMAGE, 60) as usize;

    assert_ne!(
        section_table, 0,
        "vDSO must publish its final section table"
    );
    assert_eq!(section_size, 64, "ELF64 section headers are 64 bytes");

    let text = (1..section_count)
        .map(|index| section_table + index * section_size)
        .find(|offset| le32(VDSO_IMAGE, *offset + 4) == SHT_PROGBITS)
        .expect("vDSO must publish a .text section");
    let text_start = le64(VDSO_IMAGE, text + 16);

    assert_eq!(
        exported_symbol_value("__vdso_clock_gettime"),
        text_start,
        "the resolver target must be the first instruction in .text"
    );
}

#[test]
fn highres_counter_fallback_uses_vvar_abi_contract() {
    let assembly = include_str!("vdso.S");
    let build_script = include_str!("../build.rs");

    assert_eq!(VVAR_CLOCK_MODE_OFFSET, 12);
    assert_eq!(VVAR_MULT_OFFSET, 56);
    assert_eq!(VdsoClockMode::RiscvTime as u32, 1);
    assert!(build_script.contains("VVAR_CLOCK_MODE_OFFSET"));
    assert!(build_script.contains("VVAR_MULT_OFFSET"));
    assert!(build_script.contains("VdsoClockMode::RiscvTime"));

    let highres = assembly
        .split(".Lgt_seqlock_highres:")
        .nth(1)
        .expect("high-resolution vDSO path");
    let mode = highres
        .find("VVAR_CLOCK_MODE_OFFSET")
        .expect("vDSO must read the VVAR clock mode");
    let mode_gate = highres[mode..]
        .find("bne")
        .map(|offset| mode + offset)
        .expect("unsupported clock modes must return -ENOSYS");
    let mult = highres
        .find("VVAR_MULT_OFFSET")
        .expect("vDSO must read the VVAR multiplier");
    let mult_gate = highres[mult..]
        .find("beqz")
        .map(|offset| mult + offset)
        .expect("zero VVAR multiplier must return -ENOSYS");
    let rdtime = highres.find("rdtime").expect("RV64 fast counter read");

    assert!(mode < mode_gate && mode_gate < rdtime);
    assert!(mult < mult_gate && mult_gate < rdtime);
    assert!(highres[..rdtime].contains(".Lgt_enosys"));
}

#[test]
fn coarse_counter_fallback_uses_vvar_clock_mode_gate() {
    let assembly = include_str!("vdso.S");

    let coarse = assembly
        .split(".Lgt_seqlock_coarse:")
        .nth(1)
        .expect("coarse vDSO path");
    let mode = coarse
        .find("VVAR_CLOCK_MODE_OFFSET")
        .expect("coarse vDSO path must read the VVAR clock mode");
    let mode_gate = coarse[mode..]
        .find("bne")
        .map(|offset| mode + offset)
        .expect("coarse vDSO path must reject unsupported clock modes");
    let seconds = coarse
        .find("ld      s3, 0(s1)")
        .expect("coarse vDSO path reads its realtime or monotonic base");

    assert!(mode < mode_gate && mode_gate < seconds);
    assert!(coarse[..seconds].contains(".Lgt_enosys"));

    let success = coarse
        .find("li      a0, 0")
        .expect("coarse vDSO success result");
    let done = coarse[success..]
        .find("j       .Lgt_done")
        .map(|offset| success + offset)
        .expect("coarse vDSO success must skip the -ENOSYS fallback");
    let enosys = coarse
        .find(".Lgt_enosys:")
        .expect("coarse vDSO fallback label");
    assert!(success < done && done < enosys);
}
