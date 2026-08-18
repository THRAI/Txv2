//! Build script for `tx-vdso`.
//!
//! 1. Assembles `src/vdso.S` via an available RISC-V cross-assembler.
//! 2. Extracts `.text` section bytes with objcopy.
//! 3. Parses symbol offsets from `objdump -t`.
//! 4. Builds a minimal ET_DYN ELF around the `.text` bytes.
//!
//! When the cross-toolchain is absent, emits a zero-length stub and
//! `VDSO_AVAILABLE` is `false`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tx_time::vdso::{
    VdsoClockMode, VVAR_CLOCK_MODE_OFFSET, VVAR_CYCLE_LAST_OFFSET, VVAR_MASK_OFFSET,
    VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET, VVAR_MONOTONIC_SEC_OFFSET, VVAR_MULT_OFFSET,
    VVAR_REALTIME_NSEC_SHIFTED_OFFSET, VVAR_REALTIME_SEC_OFFSET, VVAR_SEQ_OFFSET,
    VVAR_SHIFT_OFFSET,
};

const PAGE_SIZE: u64 = 0x1000;
const RV64_TEXT_ALIGNMENT: u64 = 8;
const ELF64_SECTION_HEADER_SIZE: u64 = 64;
const VVAR_DELTA: i64 = -(PAGE_SIZE as i64);
const SYM_NAMES: &[&str] = &[
    "__vdso_clock_gettime",
    "__vdso_gettimeofday",
    "__vdso_clock_getres",
    "__vdso_rt_sigreturn",
];
const VERSION_NAME: &str = "LINUX_4.15";

fn main() {
    println!("cargo:rerun-if-changed=src/vdso.S");
    println!("cargo:rerun-if-changed=vdso.ld");
    println!("cargo:rerun-if-env-changed=TX_VDSO_AS");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=HOME");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vdso_so = out_dir.join("vdso.so");
    let vdso_asm = manifest_dir.join("src").join("vdso.S");

    // `src/vdso.S` is RISC-V machine code. Embedding it on any other
    // target hands userspace a wrong-ISA vDSO — la64 glibc jumps into it
    // and the CPU decodes garbage (FPD trap inside the direct-mapped
    // image; killed every la glibc LTP boot and the libctest clock
    // tests). Until a LoongArch vdso.S exists, non-rv64 targets get the
    // stub and libc falls back to real syscalls.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let explicit_assembler = env::var_os("TX_VDSO_AS");
    if target_arch != "riscv64" && explicit_assembler.is_none() {
        emit_stub(&out_dir);
        return;
    }

    let as_path = match find_assembler(explicit_assembler) {
        Some(p) => p,
        None => {
            emit_stub(&out_dir);
            return;
        }
    };

    // 1. Assemble once to discover the VVAR-loading `auipc` within .text.
    // The second assembly below bakes the resulting PC-relative delta into
    // the image without depending on its eventual user-space placement.
    let obj = out_dir.join("vdso.o");
    let mut assembler = Command::new(&as_path);
    assembler.arg("--defsym").arg("VVAR_PC_DELTA=0");
    for (name, value) in vvar_abi_symbols() {
        assembler.arg("--defsym").arg(format!("{name}={value}"));
    }
    assembler.arg("-o").arg(&obj).arg(&vdso_asm);
    run(&mut assembler);

    let text_bin = out_dir.join("text.bin");
    let objcopy = find_companion_tool(&as_path, "objcopy")
        .expect("RISC-V objcopy not found alongside assembler or on PATH");
    run(Command::new(&objcopy)
        .arg("-O")
        .arg("binary")
        .arg("-j")
        .arg(".text")
        .arg(&obj)
        .arg(&text_bin));
    let probe_text = fs::read(&text_bin).expect("read probe text.bin");
    let probe_symbols = parse_symbols(&obj, &as_path);
    let text_start = text_start(&probe_symbols);
    let auipc_offset = vvar_auipc_offset(&probe_text[text_start as usize..]);
    let vvar_pc_delta = VVAR_DELTA - vdso_layout().text_off as i64 - auipc_offset as i64;

    // 2. Reassemble with the direct delta from the `auipc` PC to VVAR.
    let mut assembler = Command::new(&as_path);
    assembler
        .arg("--defsym")
        .arg(format!("VVAR_PC_DELTA={vvar_pc_delta}"));
    for (name, value) in vvar_abi_symbols() {
        assembler.arg("--defsym").arg(format!("{name}={value}"));
    }
    assembler.arg("-o").arg(&obj).arg(&vdso_asm);
    run(&mut assembler);

    // 3. Extract .text bytes from the final object.
    run(Command::new(objcopy)
        .arg("-O")
        .arg("binary")
        .arg("-j")
        .arg(".text")
        .arg(&obj)
        .arg(&text_bin));

    let text_bytes = fs::read(&text_bin).expect("read text.bin");

    // 4. Parse symbol offsets
    let syms = normalize_symbols(parse_symbols(&obj, &as_path), text_start);
    let (rt_sigreturn_offset, _) = find_symbol(&syms, "__vdso_rt_sigreturn");
    fs::write(
        out_dir.join("symbol_offsets.rs"),
        format!("pub const VDSO_RT_SIGRETURN_OFFSET: usize = {rt_sigreturn_offset}usize;\n"),
    )
    .expect("write vDSO symbol offsets");

    // 5. Build the ELF
    let elf = build_vdso_elf(&text_bytes[text_start as usize..], &syms);
    fs::write(&vdso_so, &elf).expect("write vdso.so");

    println!(
        "cargo:warning=[tx-vdso] {:.1} KiB vDSO ({sym_count} symbols)",
        elf.len() as f64 / 1024.0,
        sym_count = syms.len()
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_assembler(explicit: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "TX_VDSO_AS must name an absolute RISC-V assembler: {}",
            path.display()
        );
        return Some(path);
    }
    // The vDSO is freestanding assembly and has no libc ABI dependency.
    // Distribution packages commonly provide the GNU-targeted binutils
    // prefix but not a musl-prefixed assembler; accepting either avoids
    // silently replacing a valid RV64 vDSO with the syscall-only stub.
    find_tool("riscv64-linux-musl-as").or_else(|| find_tool("riscv64-linux-gnu-as"))
}

fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(path);
    }

    let candidate = PathBuf::from(env::var_os("HOME")?)
        .join(".local")
        .join("bin")
        .join(name);
    candidate.is_file().then_some(candidate)
}

fn find_companion_tool(assembler: &Path, tool: &str) -> Option<PathBuf> {
    let assembler_name = assembler.file_name()?.to_string_lossy();
    let prefix = assembler_name.strip_suffix("as")?;
    let sibling = assembler.with_file_name(format!("{prefix}{tool}"));
    sibling
        .is_file()
        .then_some(sibling)
        .or_else(|| find_tool(&format!("riscv64-linux-musl-{tool}")))
}

/// Values read by `vdso.S`, obtained from the sole `tx-time::vdso::VvarData`
/// ABI rather than duplicated as a second layout definition.
fn vvar_abi_symbols() -> [(&'static str, usize); 11] {
    [
        ("VVAR_SEQ_OFFSET", VVAR_SEQ_OFFSET),
        ("VVAR_CLOCK_MODE_OFFSET", VVAR_CLOCK_MODE_OFFSET),
        ("VVAR_REALTIME_SEC_OFFSET", VVAR_REALTIME_SEC_OFFSET),
        (
            "VVAR_REALTIME_NSEC_SHIFTED_OFFSET",
            VVAR_REALTIME_NSEC_SHIFTED_OFFSET,
        ),
        ("VVAR_MONOTONIC_SEC_OFFSET", VVAR_MONOTONIC_SEC_OFFSET),
        (
            "VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET",
            VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET,
        ),
        ("VVAR_CYCLE_LAST_OFFSET", VVAR_CYCLE_LAST_OFFSET),
        ("VVAR_MULT_OFFSET", VVAR_MULT_OFFSET),
        ("VVAR_SHIFT_OFFSET", VVAR_SHIFT_OFFSET),
        ("VVAR_MASK_OFFSET", VVAR_MASK_OFFSET),
        (
            "VDSO_CLOCK_MODE_RISCV_TIME",
            VdsoClockMode::RiscvTime as u32 as usize,
        ),
    ]
}

fn emit_stub(out_dir: &Path) {
    println!("cargo:warning=[tx-vdso] RISC-V assembler not found; stub");
    fs::write(out_dir.join("vdso.so"), []).unwrap();
    fs::write(
        out_dir.join("symbol_offsets.rs"),
        "pub const VDSO_RT_SIGRETURN_OFFSET: usize = 0;\n",
    )
    .unwrap();
    println!("cargo:rustc-cfg=vdso_stub");
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("failed to run tool");
    if !out.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        panic!("tool failed");
    }
}

fn vvar_auipc_offset(text: &[u8]) -> usize {
    text.chunks_exact(4)
        .position(|instruction| instruction == [0x17, 0x04, 0x00, 0x00])
        .map(|index| index * 4)
        .expect("vdso.S must contain `auipc s0, 0` for the VVAR base")
}

fn parse_symbols(obj: &Path, assembler: &Path) -> Vec<(String, u64, u64)> {
    let objdump = find_companion_tool(assembler, "objdump").expect("RISC-V objdump not found");
    let out = Command::new(objdump)
        .arg("-t")
        .arg(obj)
        .output()
        .expect("objdump -t");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut syms = Vec::new();
    for line in text.lines() {
        // Lines look like:
        // 0000000000000004 g     F .text  00000000000001d0 __vdso_clock_gettime
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 6 && parts[2] == "F" && parts[3] == ".text" {
            if let (Ok(addr), Ok(size)) = (
                u64::from_str_radix(parts[0], 16),
                u64::from_str_radix(parts[4], 16),
            ) {
                let name = parts[5].to_string();
                syms.push((name, addr, size));
            }
        }
    }
    syms
}

fn text_start(symbols: &[(String, u64, u64)]) -> u64 {
    symbols
        .iter()
        .map(|(_, address, _)| *address)
        .min()
        .expect("vdso.S must export at least one .text function")
}

fn normalize_symbols(symbols: Vec<(String, u64, u64)>, text_start: u64) -> Vec<(String, u64, u64)> {
    symbols
        .into_iter()
        .map(|(name, address, size)| {
            (
                name,
                address
                    .checked_sub(text_start)
                    .expect("vDSO symbol precedes its .text start"),
                size,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ELF builder
// ---------------------------------------------------------------------------

struct VdsoLayout {
    hash_off: u64,
    dynsym_off: u64,
    dynstr_off: u64,
    versym_off: u64,
    verdef_off: u64,
    text_off: u64,
}

fn vdso_layout() -> VdsoLayout {
    let dynstr_size =
        1 + SYM_NAMES.iter().map(|name| name.len() + 1).sum::<usize>() + VERSION_NAME.len() + 1;
    let nchain = SYM_NAMES.len() + 1;
    let hash_size = (2 + 5 + nchain) as u64 * 4;
    let dynsym_size = nchain as u64 * 24;
    let versym_size = nchain as u64 * 2;
    let load_start = 64 + 2 * 56;
    let hash_off = load_start;
    let dynsym_off = hash_off + hash_size;
    let dynstr_off = dynsym_off + dynsym_size;
    let versym_off = dynstr_off + dynstr_size as u64;
    let verdef_off = versym_off + versym_size;
    let text_off = align_up(verdef_off + 28, RV64_TEXT_ALIGNMENT);
    VdsoLayout {
        hash_off,
        dynsym_off,
        dynstr_off,
        versym_off,
        verdef_off,
        text_off,
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

fn build_vdso_elf(text: &[u8], symbols: &[(String, u64, u64)]) -> Vec<u8> {
    let layout = vdso_layout();

    // Build .dynstr
    let mut dynstr: Vec<u8> = vec![0u8];
    let mut str_offsets: Vec<u32> = Vec::new();
    for name in SYM_NAMES {
        str_offsets.push(dynstr.len() as u32);
        dynstr.extend_from_slice(name.as_bytes());
        dynstr.push(0);
    }
    let version_offset = dynstr.len() as u32;
    dynstr.extend_from_slice(VERSION_NAME.as_bytes());
    dynstr.push(0);
    assert_eq!(layout.dynstr_off + dynstr.len() as u64, layout.versym_off);

    // Build .dynsym (STN_UNDEF plus one entry per exported function).
    let mut dynsym = vec![0u8; (SYM_NAMES.len() + 1) * 24];
    for i in 0..SYM_NAMES.len() {
        let base = 24 + i * 24;
        // st_name
        let off = str_offsets[i];
        dynsym[base..base + 4].copy_from_slice(&off.to_le_bytes());
        // st_info = STB_GLOBAL | STT_FUNC = 0x12
        dynsym[base + 4] = 0x12;
        // st_other = STV_DEFAULT
        dynsym[base + 5] = 0;
        // st_shndx = .text. Section 1 is emitted below and starts at text_off.
        dynsym[base + 6..base + 8].copy_from_slice(&1u16.to_le_bytes());
        // st_value — symbol offset in .text
        let (addr, size) = find_symbol(symbols, SYM_NAMES[i]);
        dynsym[base + 8..base + 16].copy_from_slice(&addr.to_le_bytes());
        dynsym[base + 16..base + 24].copy_from_slice(&size.to_le_bytes());
    }

    // Build .hash (SysV)
    let elf_hash = |name: &[u8]| -> u32 {
        let mut h: u32 = 0;
        for &c in name {
            h = h.wrapping_shl(4).wrapping_add(c as u32);
            let g = h & 0xF000_0000;
            if g != 0 {
                h ^= g >> 24;
            }
            h &= !g;
        }
        h
    };
    let nbucket: u32 = 5;
    let nchain = (SYM_NAMES.len() + 1) as u32;
    let mut buckets = vec![0u32; nbucket as usize];
    let mut chains = vec![0u32; nchain as usize];
    for (i, name) in SYM_NAMES.iter().enumerate() {
        let h = elf_hash(name.as_bytes()) % nbucket;
        let sym_idx = (i + 1) as u32;
        if buckets[h as usize] == 0 {
            buckets[h as usize] = sym_idx;
        } else {
            let mut cur = buckets[h as usize] as usize;
            while chains[cur] != 0 {
                cur = chains[cur] as usize;
            }
            chains[cur] = sym_idx;
        }
    }
    let mut hash_bytes = Vec::new();
    hash_bytes.extend_from_slice(&nbucket.to_le_bytes());
    hash_bytes.extend_from_slice(&nchain.to_le_bytes());
    for b in &buckets {
        hash_bytes.extend_from_slice(&b.to_le_bytes());
    }
    for c in &chains {
        hash_bytes.extend_from_slice(&c.to_le_bytes());
    }

    let mut versym = vec![0u8; (SYM_NAMES.len() + 1) * 2];
    for index in 1..=SYM_NAMES.len() {
        versym[index * 2..index * 2 + 2].copy_from_slice(&2u16.to_le_bytes());
    }

    // Elf64_Verdef (20 B) followed by one Elf64_Verdaux (8 B).
    let mut verdef = vec![0u8; 28];
    verdef[0..2].copy_from_slice(&1u16.to_le_bytes());
    verdef[4..6].copy_from_slice(&2u16.to_le_bytes());
    verdef[6..8].copy_from_slice(&1u16.to_le_bytes());
    verdef[8..12].copy_from_slice(&elf_hash(VERSION_NAME.as_bytes()).to_le_bytes());
    verdef[12..16].copy_from_slice(&20u32.to_le_bytes());
    verdef[20..24].copy_from_slice(&version_offset.to_le_bytes());

    assert_eq!(layout.hash_off + hash_bytes.len() as u64, layout.dynsym_off);
    assert_eq!(layout.dynsym_off + dynsym.len() as u64, layout.dynstr_off);
    assert_eq!(layout.versym_off + versym.len() as u64, layout.verdef_off);
    assert!(layout.verdef_off + verdef.len() as u64 <= layout.text_off);
    assert_eq!(layout.text_off % RV64_TEXT_ALIGNMENT, 0);
    let text_sz = text.len() as u64;

    // .dynamic has every loader discovery record and deliberately no
    // relocation records: the image is position-independent by construction.
    let dynamic_off = layout.text_off + text_sz;
    let dynamic_sz: u64 = 9 * 16;
    let mut dynamic = Vec::new();
    let dyn_entry = |tag: i64, val: u64, out: &mut Vec<u8>| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    };
    dyn_entry(6, layout.dynsym_off, &mut dynamic);
    dyn_entry(5, layout.dynstr_off, &mut dynamic);
    dyn_entry(4, layout.hash_off, &mut dynamic);
    dyn_entry(10, dynstr.len() as u64, &mut dynamic);
    dyn_entry(11, 24, &mut dynamic);
    dyn_entry(0x6fff_fff0, layout.versym_off, &mut dynamic);
    dyn_entry(0x6fff_fffc, layout.verdef_off, &mut dynamic);
    dyn_entry(0x6fff_fffd, 1, &mut dynamic);
    dyn_entry(0, 0, &mut dynamic);

    let shstrtab = b"\0.text\0.shstrtab\0";
    let shstrtab_off = dynamic_off + dynamic_sz;
    let shoff = align_up(shstrtab_off + shstrtab.len() as u64, 8);
    let section_count = 3u16;
    let total = shoff + u64::from(section_count) * ELF64_SECTION_HEADER_SIZE;
    let padded = total.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut out = Vec::with_capacity(padded as usize);

    // ELF header
    out.extend_from_slice(b"\x7fELF"); // magic
    out.push(2); // ELFCLASS64
    out.push(1); // little-endian
    out.push(1); // version
    out.push(3); // ELFOSABI_LINUX
    out.extend_from_slice(&[0u8; 8]); // padding
    out.extend_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out.extend_from_slice(&243u16.to_le_bytes()); // EM_RISCV
    out.extend_from_slice(&1u32.to_le_bytes()); // version
    out.extend_from_slice(&0u64.to_le_bytes()); // entry
    out.extend_from_slice(&64u64.to_le_bytes()); // phoff
    out.extend_from_slice(&shoff.to_le_bytes()); // shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&64u16.to_le_bytes()); // ehsize
    out.extend_from_slice(&56u16.to_le_bytes()); // phentsize
    out.extend_from_slice(&2u16.to_le_bytes()); // phnum
    out.extend_from_slice(&(ELF64_SECTION_HEADER_SIZE as u16).to_le_bytes()); // shentsize
    out.extend_from_slice(&section_count.to_le_bytes()); // shnum
    out.extend_from_slice(&2u16.to_le_bytes()); // shstrndx

    // PT_LOAD
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes()); // R|X
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&padded.to_le_bytes());
    out.extend_from_slice(&padded.to_le_bytes());
    out.extend_from_slice(&PAGE_SIZE.to_le_bytes());

    // PT_DYNAMIC
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes()); // R
    out.extend_from_slice(&dynamic_off.to_le_bytes());
    out.extend_from_slice(&dynamic_off.to_le_bytes());
    out.extend_from_slice(&dynamic_off.to_le_bytes());
    out.extend_from_slice(&dynamic_sz.to_le_bytes());
    out.extend_from_slice(&dynamic_sz.to_le_bytes());
    out.extend_from_slice(&8u64.to_le_bytes());

    // Loadable content
    out.extend_from_slice(&hash_bytes);
    out.extend_from_slice(&dynsym);
    out.extend_from_slice(&dynstr);
    out.extend_from_slice(&versym);
    out.extend_from_slice(&verdef);
    out.resize(layout.text_off as usize, 0);
    out.extend_from_slice(text);
    out.extend_from_slice(&dynamic);
    out.extend_from_slice(shstrtab);
    out.resize(shoff as usize, 0);

    // Section 0 is the required null header.
    out.resize(out.len() + ELF64_SECTION_HEADER_SIZE as usize, 0);

    // .text: the first exported function starts here, so dynsym st_value is
    // both the resolver input and the first executable instruction address.
    out.extend_from_slice(&1u32.to_le_bytes()); // sh_name = ".text"
    out.extend_from_slice(&1u32.to_le_bytes()); // SHT_PROGBITS
    out.extend_from_slice(&0x6u64.to_le_bytes()); // SHF_ALLOC | SHF_EXECINSTR
    out.extend_from_slice(&layout.text_off.to_le_bytes()); // sh_addr
    out.extend_from_slice(&layout.text_off.to_le_bytes()); // sh_offset
    out.extend_from_slice(&text_sz.to_le_bytes()); // sh_size
    out.extend_from_slice(&0u32.to_le_bytes()); // sh_link
    out.extend_from_slice(&0u32.to_le_bytes()); // sh_info
    out.extend_from_slice(&RV64_TEXT_ALIGNMENT.to_le_bytes()); // sh_addralign
    out.extend_from_slice(&0u64.to_le_bytes()); // sh_entsize

    // .shstrtab
    out.extend_from_slice(&7u32.to_le_bytes()); // sh_name = ".shstrtab"
    out.extend_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&shstrtab_off.to_le_bytes()); // sh_addr
    out.extend_from_slice(&shstrtab_off.to_le_bytes()); // sh_offset
    out.extend_from_slice(&(shstrtab.len() as u64).to_le_bytes()); // sh_size
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());

    assert_eq!(out.len() as u64, total);

    // Pad to page boundary
    while (out.len() as u64) < padded {
        out.push(0);
    }

    out
}

fn find_symbol(symbols: &[(String, u64, u64)], name: &str) -> (u64, u64) {
    for (n, address, size) in symbols {
        if n == name {
            return (vdso_layout().text_off + *address, *size);
        }
    }
    panic!("required vDSO symbol {name} was not emitted by vdso.S");
}
