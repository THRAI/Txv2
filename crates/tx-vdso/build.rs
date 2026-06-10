//! Build script for `tx-vdso`.
//!
//! 1. Assembles `src/vdso.S` via the RISC-V musl cross-assembler.
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

fn main() {
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
    if target_arch != "riscv64" {
        emit_stub(&out_dir);
        return;
    }

    let as_path = match find_tool("riscv64-linux-musl-as") {
        Some(p) => p,
        None => {
            emit_stub(&out_dir);
            return;
        }
    };

    // 1. Assemble
    let obj = out_dir.join("vdso.o");
    run(Command::new(&as_path).arg("-o").arg(&obj).arg(&vdso_asm));

    // 2. Extract .text bytes
    let text_bin = out_dir.join("text.bin");
    let objcopy =
        find_tool("riscv64-linux-musl-objcopy").expect("objcopy not found alongside assembler");
    run(Command::new(&objcopy)
        .arg("-O")
        .arg("binary")
        .arg("-j")
        .arg(".text")
        .arg(&obj)
        .arg(&text_bin));

    let text_bytes = fs::read(&text_bin).expect("read text.bin");

    // 3. Parse symbol offsets
    let syms = parse_symbols(&obj);

    // 4. Build the ELF
    let elf = build_vdso_elf(&text_bytes, &syms);
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

fn find_tool(name: &str) -> Option<String> {
    Command::new("which").arg(name).output().ok().and_then(|o| {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            Some(s.trim().to_string())
        } else {
            None
        }
    })
}

fn emit_stub(out_dir: &Path) {
    println!("cargo:warning=[tx-vdso] RISC-V assembler not found; stub");
    fs::write(out_dir.join("vdso.so"), []).unwrap();
    println!("cargo:rustc-cfg=vdso_stub");
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("failed to run tool");
    if !out.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        panic!("tool failed");
    }
}

fn parse_symbols(obj: &PathBuf) -> Vec<(String, u64)> {
    let objdump = find_tool("riscv64-linux-musl-objdump").expect("objdump not found");
    let out = Command::new(&objdump)
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
            if let Ok(addr) = u64::from_str_radix(parts[0], 16) {
                let name = parts[5].to_string();
                syms.push((name, addr));
            }
        }
    }
    syms
}

// ---------------------------------------------------------------------------
// ELF builder (simplified — only updates .text bytes and symbol table)
// ---------------------------------------------------------------------------

fn build_vdso_elf(text: &[u8], symbols: &[(String, u64)]) -> Vec<u8> {
    let page_size: u64 = 0x1000;

    // --- Symbol names ---
    let sym_names: &[&str] = &[
        "__vdso_clock_gettime",
        "__vdso_gettimeofday",
        "__vdso_clock_getres",
    ];

    // Build .dynstr
    let mut dynstr: Vec<u8> = vec![0u8];
    let mut str_offsets: Vec<u32> = Vec::new();
    for name in sym_names {
        str_offsets.push(dynstr.len() as u32);
        dynstr.extend_from_slice(name.as_bytes());
        dynstr.push(0);
    }

    // Build .dynsym (4 entries × 24 bytes = 96)
    let mut dynsym = vec![0u8; 4 * 24];
    for i in 0..3 {
        let base = 24 + i * 24;
        // st_name
        let off = str_offsets[i];
        dynsym[base..base + 4].copy_from_slice(&off.to_le_bytes());
        // st_info = STB_GLOBAL | STT_FUNC = 0x12
        dynsym[base + 4] = 0x12;
        // st_other = STV_DEFAULT
        dynsym[base + 5] = 0;
        // st_shndx = SHN_ABS
        dynsym[base + 6..base + 8].copy_from_slice(&0xFFF1u16.to_le_bytes());
        // st_value — symbol offset in .text
        let addr = find_symbol_addr(symbols, sym_names[i]);
        dynsym[base + 8..base + 16].copy_from_slice(&addr.to_le_bytes());
        // st_size — use the .size directive value (we hardcode rough sizes)
        let sz: u64 = match sym_names[i] {
            "__vdso_clock_gettime" => 0x1d0,
            "__vdso_gettimeofday" => 0x70,
            "__vdso_clock_getres" => 0x5c,
            _ => 8,
        };
        dynsym[base + 16..base + 24].copy_from_slice(&sz.to_le_bytes());
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
    let nchain: u32 = 3;
    let mut buckets = vec![0u32; nbucket as usize];
    let mut chains = vec![0u32; nchain as usize];
    for (i, name) in sym_names.iter().enumerate() {
        let h = elf_hash(name.as_bytes()) % nbucket;
        let sym_idx = (i + 1) as u32;
        if buckets[h as usize] == 0 {
            buckets[h as usize] = sym_idx;
        } else {
            let mut cur = buckets[h as usize] as usize;
            while chains[cur - 1] != 0 {
                cur = chains[cur - 1] as usize;
            }
            chains[cur - 1] = sym_idx;
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

    // File layout
    let header_size: u64 = 64;
    let phdr_size: u64 = 2 * 56;
    let load_start: u64 = header_size + phdr_size;

    let hash_off = load_start;
    let hash_sz = hash_bytes.len() as u64;
    let dynsym_off = hash_off + hash_sz;
    let dynsym_sz = dynsym.len() as u64;
    let dynstr_off = dynsym_off + dynsym_sz;
    let dynstr_sz = dynstr.len() as u64;
    let text_off = dynstr_off + dynstr_sz;
    let text_sz = text.len() as u64;

    // .dynamic: DT_SYMTAB, DT_STRTAB, DT_HASH, DT_STRSZ, DT_NULL
    let dynamic_off = text_off + text_sz;
    let dynamic_sz: u64 = 5 * 16;
    let mut dynamic = Vec::new();
    let dyn_entry = |tag: i64, val: u64, out: &mut Vec<u8>| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    };
    dyn_entry(6, dynsym_off, &mut dynamic);
    dyn_entry(5, dynstr_off, &mut dynamic);
    dyn_entry(4, hash_off, &mut dynamic);
    dyn_entry(11, dynstr_sz, &mut dynamic);
    dyn_entry(0, 0, &mut dynamic);

    let total = dynamic_off + dynamic_sz;
    let padded = total.div_ceil(page_size) * page_size;
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
    out.extend_from_slice(&0u64.to_le_bytes()); // shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&64u16.to_le_bytes()); // ehsize
    out.extend_from_slice(&56u16.to_le_bytes()); // phentsize
    out.extend_from_slice(&2u16.to_le_bytes()); // phnum
    out.extend_from_slice(&0u16.to_le_bytes()); // shentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // shstrndx

    // PT_LOAD
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes()); // R|X
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&padded.to_le_bytes());
    out.extend_from_slice(&padded.to_le_bytes());
    out.extend_from_slice(&page_size.to_le_bytes());

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
    out.extend_from_slice(text);
    out.extend_from_slice(&dynamic);

    // Pad to page boundary
    while (out.len() as u64) < padded {
        out.push(0);
    }

    out
}

fn find_symbol_addr(symbols: &[(String, u64)], name: &str) -> u64 {
    for (n, a) in symbols {
        if n == name {
            return *a;
        }
    }
    // Fallback: use stub offset (each 8 bytes)
    match name {
        "__vdso_clock_gettime" => 4,
        "__vdso_gettimeofday" => 12,
        "__vdso_clock_getres" => 20,
        _ => 0,
    }
}
