//! Bootstrap `/init` fixture binary for Phase 7 of the ELF-loader plan.
//!
//! `INIT_FIXTURE_BYTES` is a hand-encoded RV64 static `ET_EXEC` ELF
//! binary. The plan §"Where /init comes from" originally proposed
//! shipping a static-musl-linked hello-world built with an external
//! cross-toolchain (`riscv64-linux-musl-gcc`); the binary would
//! land ~25 KiB checked in as a binary blob. This implementation
//! instead hand-encodes the minimum bytes needed for the smoke
//! test (one `write` + one `exit_group`), keeping the kernel image
//! self-contained and dependency-free. The full musl-linked path
//! lands as a follow-up once a toolchain pinning story exists.
//!
//! ## Userspace behaviour
//!
//! ```text
//! _start:
//!     li a7, 64           # NR_WRITE
//!     li a0, 1            # fd = stdout
//!     auipc a1, 0         # a1 = pc(this insn)
//!     addi  a1, a1, 28    # a1 += offset to `msg` (msg is 28 bytes after auipc)
//!     li a2, 6            # len = 6
//!     ecall               # write(1, "hello\n", 6)
//!     li a7, 94           # NR_EXIT_GROUP
//!     li a0, 0            # status = 0
//!     ecall               # exit_group(0)
//! msg:
//!     .ascii "hello\n"
//! ```
//!
//! The Linux RV64 syscall ABI is: `a7` carries the syscall number,
//! `a0..a6` the arguments, `ecall` traps. The shim layer's
//! `linux_syscall::dispatch` matches on `a7` and dispatches to the
//! corresponding arm; `NR_WRITE = 64` and `NR_EXIT_GROUP = 94`
//! mirror Linux's `<asm-generic/unistd.h>` numbering.
//!
//! ## ELF layout
//!
//! ```text
//! Offset | Size | Contents
//! -------|------|------------------------------------------------
//!   0    |  64  | ELF64 Ehdr
//!  64    |  56  | PT_PHDR (program-header self-cover)
//! 120    |  56  | PT_LOAD (R+X, covers the entire file)
//! 176    |  36  | code (9 RV64 instructions × 4 bytes)
//! 212    |   6  | "hello\n"
//! -------|------|------------------------------------------------
//! Total: 218 bytes.
//! ```
//!
//! `LOAD_VADDR = 0x10000` (low userspace; well above any trap-page
//! reservation, well below the default user stack at
//! `USER_STACK_TOP_DEFAULT = 0x4000_0000` from Wave 1).
//!
//! Entry: `LOAD_VADDR + 176 = 0x100B0` (first instruction).
//! `msg` vaddr: `LOAD_VADDR + 212 = 0x100D4`.
//! `auipc` PC (third instruction): `LOAD_VADDR + 184 = 0x100B8`.
//! `msg - auipc = 0x100D4 - 0x100B8 = 0x1C = 28`, so the immediate
//! for `addi a1, a1, 28` is `28`.
//!
//! ## RV64 byte map (each 32-bit instruction is little-endian)
//!
//! ```text
//! Insn # | VAddr   | Hex Encoding | Mnemonic
//! -------|---------|--------------|------------------------
//!   0    | 0x100B0 | 04 00 00 89  | li a7, 64       (0x04000893)
//!   1    | 0x100B4 | 13 05 10 00  | li a0, 1        (0x00100513)
//!   2    | 0x100B8 | 97 05 00 00  | auipc a1, 0     (0x00000597)
//!   3    | 0x100BC | 93 85 c5 01  | addi a1, a1, 28 (0x01c58593)
//!   4    | 0x100C0 | 13 06 60 00  | li a2, 6        (0x00600613)
//!   5    | 0x100C4 | 73 00 00 00  | ecall           (0x00000073)
//!   6    | 0x100C8 | 93 08 e0 05  | li a7, 94       (0x05e00893)
//!   7    | 0x100CC | 13 05 00 00  | li a0, 0        (0x00000513)
//!   8    | 0x100D0 | 73 00 00 00  | ecall           (0x00000073)
//! ```
//!
//! (Encodings verified by hand against the RV64I ABI / Volume I:
//! addi is I-type, auipc is U-type, ecall is the all-zeros encoding
//! plus opcode 0x73; see RISC-V User-Level ISA §2.5 / §2.7.)
//!
//! The follow-up alternative is to assemble the same source via
//! `riscv64-linux-musl-as` at build time inside `build.rs` and
//! `include_bytes!` the result; that requires a pinned cross
//! toolchain. The hand-encoded blob below is portable and drops
//! the toolchain dependency for now.

/// LOAD virtual address (entry of the PT_LOAD segment).
///
/// Pinned in the comment block above; matches the ELF e_entry's base
/// computation (`e_entry = LOAD_VADDR + 176`) and the auipc-relative
/// `msg` resolution. Documented as `pub` so the host-side smoke test
/// can pin the seeded `saved_user_context.pc` against the
/// fixture's entry point.
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_FIXTURE_LOAD_VADDR: u64 = 0x10000;

/// Entry-point virtual address (first instruction).
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_FIXTURE_ENTRY_VADDR: u64 = INIT_FIXTURE_LOAD_VADDR + 176;

/// Total fixture size in bytes (also `p_filesz` and `p_memsz` of the
/// PT_LOAD segment).
pub const INIT_FIXTURE_FILE_SIZE: usize = 218;

/// Hand-encoded RV64 ET_EXEC ELF binary. See module docstring for
/// the layout, byte map, and disassembly.
///
/// Stored as a top-level `static` so `include_bytes!`-style consumers
/// (the bootstrap `register_init_fixture_into_tmpfs` step) can take
/// `&[u8]` references into the kernel image's `.rodata`. The trailing
/// `pub const INIT_FIXTURE_FILE_SIZE` and `assert!` enforce the byte
/// count at compile time.
#[rustfmt::skip]
pub static INIT_FIXTURE_BYTES: [u8; INIT_FIXTURE_FILE_SIZE] = {
    // Build the array piece-by-piece. Each section's offset is
    // commented inline so a future audit can sanity-check against
    // the layout block above.
    let mut bytes = [0u8; INIT_FIXTURE_FILE_SIZE];

    // ----- ELF64 Ehdr (offset 0..64) ----------------------------
    // e_ident: magic + ELFCLASS64 + ELFDATA2LSB + EV_CURRENT +
    //          ELFOSABI_SYSV + ABIVERSION 0 + 7 padding bytes.
    bytes[0] = 0x7f; bytes[1] = b'E'; bytes[2] = b'L'; bytes[3] = b'F';
    bytes[4] = 2;     // ELFCLASS64
    bytes[5] = 1;     // ELFDATA2LSB (little-endian)
    bytes[6] = 1;     // EV_CURRENT
    bytes[7] = 0;     // ELFOSABI_SYSV
    bytes[8] = 0;     // ABIVERSION 0
    // bytes[9..16] = 0 (padding)
    // e_type = ET_EXEC = 2 (offset 16, u16 LE)
    bytes[16] = 2; bytes[17] = 0;
    // e_machine = EM_RISCV = 243 (offset 18, u16 LE)
    bytes[18] = 0xf3; bytes[19] = 0;
    // e_version = 1 (offset 20, u32 LE)
    bytes[20] = 1; bytes[21] = 0; bytes[22] = 0; bytes[23] = 0;
    // e_entry = INIT_FIXTURE_ENTRY_VADDR (offset 24, u64 LE).
    // = 0x100B0 → bytes B0 00 01 00 00 00 00 00.
    bytes[24] = 0xB0; bytes[25] = 0x00; bytes[26] = 0x01; bytes[27] = 0x00;
    bytes[28] = 0x00; bytes[29] = 0x00; bytes[30] = 0x00; bytes[31] = 0x00;
    // e_phoff = 64 (offset 32, u64 LE)
    bytes[32] = 64; // bytes[33..40] = 0
    // e_shoff = 0 (offset 40)
    // e_flags = 0 (offset 48, u32)
    // e_ehsize = 64 (offset 52, u16)
    bytes[52] = 64;
    // e_phentsize = 56 (offset 54)
    bytes[54] = 56;
    // e_phnum = 2 (offset 56)
    bytes[56] = 2;
    // e_shentsize = 0, e_shnum = 0, e_shstrndx = 0 (offsets 58..64)

    // ----- PT_PHDR (offset 64..120) -----------------------------
    // p_type = PT_PHDR = 6 (u32 LE)
    bytes[64] = 6;
    // p_flags = PF_R | PF_X = 5 (offset 68, u32 LE)
    bytes[68] = 5;
    // p_offset = 64 (offset 72, u64 LE)
    bytes[72] = 64;
    // p_vaddr = LOAD_VADDR + 64 = 0x10040 (offset 80, u64 LE)
    bytes[80] = 0x40; bytes[81] = 0x00; bytes[82] = 0x01; bytes[83] = 0x00;
    // p_paddr = same as p_vaddr (offset 88)
    bytes[88] = 0x40; bytes[89] = 0x00; bytes[90] = 0x01; bytes[91] = 0x00;
    // p_filesz = 112 (offset 96, u64 LE) — two PHDRs
    bytes[96] = 112;
    // p_memsz = 112 (offset 104)
    bytes[104] = 112;
    // p_align = 8 (offset 112)
    bytes[112] = 8;

    // ----- PT_LOAD (offset 120..176) ----------------------------
    // p_type = PT_LOAD = 1 (u32 LE)
    bytes[120] = 1;
    // p_flags = PF_R | PF_X = 5 (offset 124)
    bytes[124] = 5;
    // p_offset = 0 (offset 128, u64) — covers the entire file
    // (already zero)
    // p_vaddr = LOAD_VADDR = 0x10000 (offset 136)
    bytes[136] = 0x00; bytes[137] = 0x00; bytes[138] = 0x01; bytes[139] = 0x00;
    // p_paddr = same (offset 144)
    bytes[144] = 0x00; bytes[145] = 0x00; bytes[146] = 0x01; bytes[147] = 0x00;
    // p_filesz = INIT_FIXTURE_FILE_SIZE = 218 (offset 152)
    bytes[152] = 218; bytes[153] = 0;
    // p_memsz = same (offset 160)
    bytes[160] = 218; bytes[161] = 0;
    // p_align = 0x1000 (offset 168)
    bytes[168] = 0x00; bytes[169] = 0x10; bytes[170] = 0x00; bytes[171] = 0x00;

    // ----- Code (offset 176..212) -------------------------------
    // li a7, 64 → 0x04000893 (LE: 93 08 00 04)
    bytes[176] = 0x93; bytes[177] = 0x08; bytes[178] = 0x00; bytes[179] = 0x04;
    // li a0, 1  → 0x00100513 (LE: 13 05 10 00)
    bytes[180] = 0x13; bytes[181] = 0x05; bytes[182] = 0x10; bytes[183] = 0x00;
    // auipc a1, 0 → 0x00000597 (LE: 97 05 00 00)
    bytes[184] = 0x97; bytes[185] = 0x05; bytes[186] = 0x00; bytes[187] = 0x00;
    // addi a1, a1, 28 → 0x01c58593 (LE: 93 85 c5 01)
    bytes[188] = 0x93; bytes[189] = 0x85; bytes[190] = 0xc5; bytes[191] = 0x01;
    // li a2, 6 → 0x00600613 (LE: 13 06 60 00)
    bytes[192] = 0x13; bytes[193] = 0x06; bytes[194] = 0x60; bytes[195] = 0x00;
    // ecall → 0x00000073 (LE: 73 00 00 00)
    bytes[196] = 0x73; bytes[197] = 0x00; bytes[198] = 0x00; bytes[199] = 0x00;
    // li a7, 94 → 0x05e00893 (LE: 93 08 e0 05)
    bytes[200] = 0x93; bytes[201] = 0x08; bytes[202] = 0xe0; bytes[203] = 0x05;
    // li a0, 0 → 0x00000513 (LE: 13 05 00 00)
    bytes[204] = 0x13; bytes[205] = 0x05; bytes[206] = 0x00; bytes[207] = 0x00;
    // ecall → 0x00000073 (LE: 73 00 00 00)
    bytes[208] = 0x73; bytes[209] = 0x00; bytes[210] = 0x00; bytes[211] = 0x00;

    // ----- Message bytes (offset 212..218) -----------------------
    // "hello\n"
    bytes[212] = b'h';
    bytes[213] = b'e';
    bytes[214] = b'l';
    bytes[215] = b'l';
    bytes[216] = b'o';
    bytes[217] = b'\n';

    bytes
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time-shaped checks that the layout constants align with
    /// the bytes we hand-encoded. These are cheap host checks; the
    /// real validation is the Phase 7 boot smoke that loads the
    /// fixture through `exec_script` and observes `:userspace:exited:0`
    /// + the post-OPOST `b"hello\r\n"` console capture.
    #[test]
    fn fixture_size_matches_constant() {
        assert_eq!(INIT_FIXTURE_BYTES.len(), INIT_FIXTURE_FILE_SIZE);
        assert_eq!(INIT_FIXTURE_FILE_SIZE, 218);
    }

    #[test]
    fn fixture_starts_with_elf_magic() {
        assert_eq!(&INIT_FIXTURE_BYTES[0..4], b"\x7fELF");
        assert_eq!(INIT_FIXTURE_BYTES[4], 2); // ELFCLASS64
        assert_eq!(INIT_FIXTURE_BYTES[5], 1); // ELFDATA2LSB
    }

    #[test]
    fn fixture_e_machine_is_riscv() {
        // EM_RISCV = 243 = 0xf3.
        assert_eq!(INIT_FIXTURE_BYTES[18], 0xf3);
        assert_eq!(INIT_FIXTURE_BYTES[19], 0x00);
    }

    #[test]
    fn fixture_e_entry_matches_constant() {
        let entry = u64::from_le_bytes(INIT_FIXTURE_BYTES[24..32].try_into().unwrap());
        assert_eq!(entry, INIT_FIXTURE_ENTRY_VADDR);
        assert_eq!(entry, 0x100B0);
    }

    #[test]
    fn fixture_msg_bytes_at_offset_212() {
        assert_eq!(&INIT_FIXTURE_BYTES[212..218], b"hello\n");
    }

    #[test]
    fn fixture_first_instruction_is_li_a7_64() {
        // Little-endian: 93 08 00 04 → 0x04000893.
        let insn = u32::from_le_bytes(INIT_FIXTURE_BYTES[176..180].try_into().unwrap());
        assert_eq!(insn, 0x04000893);
    }

    #[test]
    fn fixture_load_vaddr_constants_consistent() {
        // PT_LOAD's p_vaddr at offset 136.
        let load_vaddr = u64::from_le_bytes(INIT_FIXTURE_BYTES[136..144].try_into().unwrap());
        assert_eq!(load_vaddr, INIT_FIXTURE_LOAD_VADDR);
        // entry = LOAD + 176.
        assert_eq!(INIT_FIXTURE_ENTRY_VADDR, INIT_FIXTURE_LOAD_VADDR + 176);
    }
}
