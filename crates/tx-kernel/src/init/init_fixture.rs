//! Bootstrap `/init` fixture binary for the fork/clone/wait4 slice.
//!
//! Originally introduced by the ELF loader's Phase 7 as a 218-byte
//! `write("hello\n") + exit_group(0)` blob, this fixture has been
//! extended (Wave 4 of the fork/clone/wait4 slice, Open Q #4 DECIDED
//! 2026-05-06: extend in place, single source of truth) to actually
//! fork+wait+exit. The new shape exercises NR_CLONE / NR_WAIT4 /
//! NR_WRITE / NR_EXIT_GROUP end-to-end:
//!
//! ```text
//! _start:                         ; clone(SIGCHLD, 0, 0, 0, 0) — bare-SIGCHLD fork
//!     li   a7, 220                ; NR_CLONE
//!     li   a0, 17                 ; SIGCHLD termination signal
//!     li   a1, 0                  ; stack=NULL
//!     li   a2, 0                  ; parent_tid_ptr=NULL
//!     li   a3, 0                  ; tls=NULL
//!     li   a4, 0                  ; child_tid_ptr=NULL
//!     ecall                       ; parent: a0=child_pid; child: a0=0
//!     bnez a0, parent_path
//!
//! child_path:                     ; child: write("child\n") + exit_group(0)
//!     li   a7, 64                 ; NR_WRITE
//!     li   a0, 1                  ; fd=stdout
//!     auipc a1, 0
//!     addi  a1, a1, 88            ; → child_msg
//!     li   a2, 6
//!     ecall
//!     li   a7, 94                 ; NR_EXIT_GROUP
//!     li   a0, 0
//!     ecall
//!
//! parent_path:                    ; parent: wait4(-1, NULL, 0, NULL) → write("parent\n") → exit_group(0)
//!     li   a7, 260                ; NR_WAIT4
//!     addi a0, x0, -1             ; pid=-1 (any child)
//!     li   a1, 0                  ; wstatus_uaddr=NULL
//!     li   a2, 0                  ; options=0 (blocking)
//!     li   a3, 0                  ; rusage_uaddr=NULL
//!     ecall
//!     li   a7, 64                 ; NR_WRITE
//!     li   a0, 1
//!     auipc a1, 0
//!     addi  a1, a1, 34            ; → parent_msg
//!     li   a2, 7
//!     ecall
//!     li   a7, 94                 ; NR_EXIT_GROUP
//!     li   a0, 0
//!     ecall
//!
//! child_msg:  .ascii "child\n"
//! parent_msg: .ascii "parent\n"
//! ```
//!
//! The Linux RV64 syscall ABI is: `a7` carries the syscall number,
//! `a0..a6` the arguments, `ecall` traps. The shim layer's
//! `linux_syscall::dispatch` matches on `a7` and dispatches to the
//! corresponding arm. Wave 1+2+3 wired NR_CLONE, NR_WAIT4, NR_WRITE,
//! and NR_EXIT_GROUP through that dispatcher.
//!
//! ## ELF layout
//!
//! ```text
//! Offset | Size | Contents
//! -------|------|------------------------------------------------
//!   0    |  64  | ELF64 Ehdr
//!  64    |  56  | PT_PHDR (program-header self-cover)
//! 120    |  56  | PT_LOAD (R+X, covers the entire file)
//! 176    | 128  | code (32 RV64 instructions × 4 bytes)
//! 304    |   6  | "child\n"
//! 310    |   7  | "parent\n"
//! -------|------|------------------------------------------------
//! Total: 317 bytes.
//! ```
//!
//! `LOAD_VADDR = 0x10000` (low userspace; well above any trap-page
//! reservation, well below the default user stack at
//! `USER_STACK_TOP_DEFAULT = 0x4000_0000` from Wave 1 of the ELF
//! loader).
//!
//! Entry: `LOAD_VADDR + 176 = 0x100B0` (first instruction).
//! `child_msg` vaddr: `LOAD_VADDR + 304 = 0x10130`.
//! `parent_msg` vaddr: `LOAD_VADDR + 310 = 0x10136`.
//!
//! The two `auipc/addi` pairs resolve the message pointers by
//! PC-relative addressing:
//!
//! - **Child write.** `auipc` is at insn index 10 → PC =
//!   `LOAD_VADDR + 176 + 40 = 0x100D8`. `child_msg - PC = 0x10130 -
//!   0x100D8 = 0x58 = 88`, so the addi immediate is `88`.
//! - **Parent write.** `auipc` is at insn index 25 → PC =
//!   `LOAD_VADDR + 176 + 100 = 0x10114`. `parent_msg - PC = 0x10136 -
//!   0x10114 = 0x22 = 34`, so the addi immediate is `34`.
//!
//! ## RV64 byte map (each 32-bit instruction is little-endian)
//!
//! ```text
//! Insn # | VAddr   | Hex Encoding | Mnemonic
//! -------|---------|--------------|-------------------------------
//! Pre-branch (clone setup):
//!   0    | 0x100B0 | 93 08 c0 0d  | li   a7, 220       (0x0dc00893)
//!   1    | 0x100B4 | 13 05 10 01  | li   a0, 17        (0x01100513)
//!   2    | 0x100B8 | 93 05 00 00  | li   a1, 0         (0x00000593)
//!   3    | 0x100BC | 13 06 00 00  | li   a2, 0         (0x00000613)
//!   4    | 0x100C0 | 93 06 00 00  | li   a3, 0         (0x00000693)
//!   5    | 0x100C4 | 13 07 00 00  | li   a4, 0         (0x00000713)
//!   6    | 0x100C8 | 73 00 00 00  | ecall              (0x00000073)
//!   7    | 0x100CC | 63 14 05 02  | bnez a0, +40       (0x02051463)
//! Child path (PC = 0x100D0..0x100F4):
//!   8    | 0x100D0 | 93 08 00 04  | li   a7, 64        (0x04000893)
//!   9    | 0x100D4 | 13 05 10 00  | li   a0, 1         (0x00100513)
//!  10    | 0x100D8 | 97 05 00 00  | auipc a1, 0        (0x00000597)
//!  11    | 0x100DC | 93 85 85 05  | addi a1, a1, 88    (0x05858593)
//!  12    | 0x100E0 | 13 06 60 00  | li   a2, 6         (0x00600613)
//!  13    | 0x100E4 | 73 00 00 00  | ecall              (0x00000073)
//!  14    | 0x100E8 | 93 08 e0 05  | li   a7, 94        (0x05e00893)
//!  15    | 0x100EC | 13 05 00 00  | li   a0, 0         (0x00000513)
//!  16    | 0x100F0 | 73 00 00 00  | ecall              (0x00000073)
//! Parent path (PC = 0x100F4..0x10130):
//!  17    | 0x100F4 | 93 08 40 10  | li   a7, 260       (0x10400893)
//!  18    | 0x100F8 | 13 05 f0 ff  | addi a0, x0, -1    (0xfff00513)
//!  19    | 0x100FC | 93 05 00 00  | li   a1, 0         (0x00000593)
//!  20    | 0x10100 | 13 06 00 00  | li   a2, 0         (0x00000613)
//!  21    | 0x10104 | 93 06 00 00  | li   a3, 0         (0x00000693)
//!  22    | 0x10108 | 73 00 00 00  | ecall              (0x00000073)
//!  23    | 0x1010C | 93 08 00 04  | li   a7, 64        (0x04000893)
//!  24    | 0x10110 | 13 05 10 00  | li   a0, 1         (0x00100513)
//!  25    | 0x10114 | 97 05 00 00  | auipc a1, 0        (0x00000597)
//!  26    | 0x10118 | 93 85 25 02  | addi a1, a1, 34    (0x02258593)
//!  27    | 0x1011C | 13 06 70 00  | li   a2, 7         (0x00700613)
//!  28    | 0x10120 | 73 00 00 00  | ecall              (0x00000073)
//!  29    | 0x10124 | 93 08 e0 05  | li   a7, 94        (0x05e00893)
//!  30    | 0x10128 | 13 05 00 00  | li   a0, 0         (0x00000513)
//!  31    | 0x1012C | 73 00 00 00  | ecall              (0x00000073)
//! ```
//!
//! ## `bnez a0, parent_path` encoding
//!
//! `bnez rs, target` is the assembler sugar for `bne rs, x0, target`.
//! The B-type immediate splits across the instruction word as
//! `imm[12|10:5|4:1|11]`; the displacement is signed and PC-relative
//! (PC of the branch instruction itself).
//!
//! The branch sits at insn 7 (PC = `0x100CC`). The parent path
//! starts at insn 17 (PC = `0x100F4`). Displacement = `0x100F4 -
//! 0x100CC = 0x28 = 40`. 40 in binary is `0000 0010 1000`:
//!
//! ```text
//! imm bit: 12 11 10  9  8  7  6  5  4  3  2  1  0
//!          0   0  0  0  0  0  0  1  0  1  0  0  0
//! ```
//!
//! Encoding (B-type):
//! - bit[31] = imm[12] = 0
//! - bits[30:25] = imm[10:5] = 000001
//! - bits[24:20] = rs2 = 00000  (x0)
//! - bits[19:15] = rs1 = 01010  (a0 = 10)
//! - bits[14:12] = funct3 = 001 (BNE)
//! - bits[11:8]  = imm[4:1] = 0100
//! - bit[7]      = imm[11] = 0
//! - bits[6:0]   = opcode = 1100011 (BRANCH)
//!
//! Concatenated: `0_000001_00000_01010_001_0100_0_1100011` =
//! `0x0205_1463`. Little-endian bytes: `63 14 05 02`. The branch
//! falls forward 10 instructions, skipping the 9-instruction child
//! path immediately after the branch.
//!
//! ## RV64 encoding cross-checks
//!
//! - `addi` (I-type): bits[31:20]=imm[11:0], bits[19:15]=rs1,
//!   bits[14:12]=000 (funct3), bits[11:7]=rd, bits[6:0]=0010011.
//!   `li rd, k` is `addi rd, x0, k`.
//! - `addi a0, x0, -1` sign-extends `-1` to all-ones in the 12-bit
//!   immediate slot, yielding `0xfff00513`. RV64 sign-extends to
//!   `xlen` so `a0` ends up `0xFFFF_FFFF_FFFF_FFFF`, which is the
//!   bit pattern POSIX `wait4(-1, ...)` expects.
//! - `auipc rd, 0`: `lui`-style U-type; bits[31:12]=imm[31:12]=0,
//!   bits[11:7]=rd, bits[6:0]=0010111. With imm=0 the result is
//!   `pc & 0xFFFF_F000` (since `auipc` shifts the immediate left by
//!   12). Both auipc/addi sequences in this fixture pair an
//!   `auipc a1, 0` with an `addi a1, a1, +imm12` to recover the
//!   data pointer; the `auipc` deposits the high bits of its own
//!   PC into a1, then the `addi` fixes up the low 12 bits with the
//!   signed offset to the message bytes.
//! - `ecall` is the all-zeros encoding plus opcode `0x73`
//!   (`0x00000073`).
//! - All `li`-form `addi rd, x0, k` widths are within ±2047, so
//!   no `lui` prefix is needed (220, 17, 64, 94, 260, 6, 7, and
//!   the per-immediate offsets all fit).
//!
//! (Encodings cross-checked against RV64I ABI / Volume I:
//! User-Level ISA §2.5 / §2.7. The B-type derivation above mirrors
//! the standard reference encoding table.)

/// LOAD virtual address (entry of the PT_LOAD segment).
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_FIXTURE_LOAD_VADDR: u64 = 0x10000;

/// Entry-point virtual address (first instruction).
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_FIXTURE_ENTRY_VADDR: u64 = INIT_FIXTURE_LOAD_VADDR + 176;

/// Total fixture size in bytes (also `p_filesz` and `p_memsz` of the
/// PT_LOAD segment).
pub const INIT_FIXTURE_FILE_SIZE: usize = 317;

/// Byte offset (within the file) of the bnez instruction.
#[cfg(test)]
const BNEZ_FILE_OFFSET: usize = 176 + 7 * 4;

/// Byte offset (within the file) of the parent_path label.
#[cfg(test)]
const PARENT_PATH_FILE_OFFSET: usize = 176 + 17 * 4;

/// Hand-encoded RV64 ET_EXEC ELF binary. See module docstring for
/// the layout, byte map, and disassembly.
///
/// Stored as a top-level `static` so `include_bytes!`-style consumers
/// (the bootstrap `register_init_fixture_into_tmpfs` step) can take
/// `&[u8]` references into the kernel image's `.rodata`.
#[rustfmt::skip]
pub static INIT_FIXTURE_BYTES: [u8; INIT_FIXTURE_FILE_SIZE] = {
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
    // p_filesz = INIT_FIXTURE_FILE_SIZE = 317 (offset 152, u64 LE).
    // 317 = 0x13D → bytes 3D 01 00 00 00 00 00 00.
    bytes[152] = 0x3D; bytes[153] = 0x01;
    // p_memsz = same (offset 160)
    bytes[160] = 0x3D; bytes[161] = 0x01;
    // p_align = 0x1000 (offset 168)
    bytes[168] = 0x00; bytes[169] = 0x10; bytes[170] = 0x00; bytes[171] = 0x00;

    // ----- Code (offset 176..304) -------------------------------
    //
    // Pre-branch: clone(SIGCHLD, 0, 0, 0, 0) + bnez to parent_path.
    //
    // li a7, 220 → 0x0dc00893
    bytes[176] = 0x93; bytes[177] = 0x08; bytes[178] = 0xc0; bytes[179] = 0x0d;
    // li a0, 17 → 0x01100513
    bytes[180] = 0x13; bytes[181] = 0x05; bytes[182] = 0x10; bytes[183] = 0x01;
    // li a1, 0 → 0x00000593
    bytes[184] = 0x93; bytes[185] = 0x05; bytes[186] = 0x00; bytes[187] = 0x00;
    // li a2, 0 → 0x00000613
    bytes[188] = 0x13; bytes[189] = 0x06; bytes[190] = 0x00; bytes[191] = 0x00;
    // li a3, 0 → 0x00000693
    bytes[192] = 0x93; bytes[193] = 0x06; bytes[194] = 0x00; bytes[195] = 0x00;
    // li a4, 0 → 0x00000713
    bytes[196] = 0x13; bytes[197] = 0x07; bytes[198] = 0x00; bytes[199] = 0x00;
    // ecall → 0x00000073
    bytes[200] = 0x73; bytes[201] = 0x00; bytes[202] = 0x00; bytes[203] = 0x00;
    // bnez a0, +40 (parent_path) → 0x02051463
    bytes[204] = 0x63; bytes[205] = 0x14; bytes[206] = 0x05; bytes[207] = 0x02;

    // Child path: write(1, child_msg, 6) + exit_group(0).
    //
    // li a7, 64 → 0x04000893
    bytes[208] = 0x93; bytes[209] = 0x08; bytes[210] = 0x00; bytes[211] = 0x04;
    // li a0, 1 → 0x00100513
    bytes[212] = 0x13; bytes[213] = 0x05; bytes[214] = 0x10; bytes[215] = 0x00;
    // auipc a1, 0 → 0x00000597
    bytes[216] = 0x97; bytes[217] = 0x05; bytes[218] = 0x00; bytes[219] = 0x00;
    // addi a1, a1, 88 → 0x05858593  (child_msg = auipc_pc + 88)
    bytes[220] = 0x93; bytes[221] = 0x85; bytes[222] = 0x85; bytes[223] = 0x05;
    // li a2, 6 → 0x00600613
    bytes[224] = 0x13; bytes[225] = 0x06; bytes[226] = 0x60; bytes[227] = 0x00;
    // ecall → 0x00000073
    bytes[228] = 0x73; bytes[229] = 0x00; bytes[230] = 0x00; bytes[231] = 0x00;
    // li a7, 94 → 0x05e00893
    bytes[232] = 0x93; bytes[233] = 0x08; bytes[234] = 0xe0; bytes[235] = 0x05;
    // li a0, 0 → 0x00000513
    bytes[236] = 0x13; bytes[237] = 0x05; bytes[238] = 0x00; bytes[239] = 0x00;
    // ecall → 0x00000073
    bytes[240] = 0x73; bytes[241] = 0x00; bytes[242] = 0x00; bytes[243] = 0x00;

    // Parent path: wait4(-1, NULL, 0, NULL) + write(1, parent_msg, 7) + exit_group(0).
    //
    // li a7, 260 → 0x10400893
    bytes[244] = 0x93; bytes[245] = 0x08; bytes[246] = 0x40; bytes[247] = 0x10;
    // addi a0, x0, -1 → 0xfff00513
    bytes[248] = 0x13; bytes[249] = 0x05; bytes[250] = 0xf0; bytes[251] = 0xff;
    // li a1, 0 → 0x00000593
    bytes[252] = 0x93; bytes[253] = 0x05; bytes[254] = 0x00; bytes[255] = 0x00;
    // li a2, 0 → 0x00000613
    bytes[256] = 0x13; bytes[257] = 0x06; bytes[258] = 0x00; bytes[259] = 0x00;
    // li a3, 0 → 0x00000693
    bytes[260] = 0x93; bytes[261] = 0x06; bytes[262] = 0x00; bytes[263] = 0x00;
    // ecall → 0x00000073
    bytes[264] = 0x73; bytes[265] = 0x00; bytes[266] = 0x00; bytes[267] = 0x00;
    // li a7, 64 → 0x04000893
    bytes[268] = 0x93; bytes[269] = 0x08; bytes[270] = 0x00; bytes[271] = 0x04;
    // li a0, 1 → 0x00100513
    bytes[272] = 0x13; bytes[273] = 0x05; bytes[274] = 0x10; bytes[275] = 0x00;
    // auipc a1, 0 → 0x00000597
    bytes[276] = 0x97; bytes[277] = 0x05; bytes[278] = 0x00; bytes[279] = 0x00;
    // addi a1, a1, 34 → 0x02258593  (parent_msg = auipc_pc + 34)
    bytes[280] = 0x93; bytes[281] = 0x85; bytes[282] = 0x25; bytes[283] = 0x02;
    // li a2, 7 → 0x00700613
    bytes[284] = 0x13; bytes[285] = 0x06; bytes[286] = 0x70; bytes[287] = 0x00;
    // ecall → 0x00000073
    bytes[288] = 0x73; bytes[289] = 0x00; bytes[290] = 0x00; bytes[291] = 0x00;
    // li a7, 94 → 0x05e00893
    bytes[292] = 0x93; bytes[293] = 0x08; bytes[294] = 0xe0; bytes[295] = 0x05;
    // li a0, 0 → 0x00000513
    bytes[296] = 0x13; bytes[297] = 0x05; bytes[298] = 0x00; bytes[299] = 0x00;
    // ecall → 0x00000073
    bytes[300] = 0x73; bytes[301] = 0x00; bytes[302] = 0x00; bytes[303] = 0x00;

    // ----- Message bytes (offset 304..317) -----------------------
    // child_msg = "child\n" at offset 304..310.
    bytes[304] = b'c';
    bytes[305] = b'h';
    bytes[306] = b'i';
    bytes[307] = b'l';
    bytes[308] = b'd';
    bytes[309] = b'\n';
    // parent_msg = "parent\n" at offset 310..317.
    bytes[310] = b'p';
    bytes[311] = b'a';
    bytes[312] = b'r';
    bytes[313] = b'e';
    bytes[314] = b'n';
    bytes[315] = b't';
    bytes[316] = b'\n';

    bytes
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time-shaped checks that the layout constants align with
    /// the bytes we hand-encoded. The Wave 4 fork+wait+exit fixture
    /// is 317 bytes (vs the original ELF-loader 218-byte
    /// hello-world). Every pin test below was reviewed against the
    /// new contents.
    #[test]
    fn fixture_size_matches_constant() {
        assert_eq!(INIT_FIXTURE_BYTES.len(), INIT_FIXTURE_FILE_SIZE);
        assert_eq!(INIT_FIXTURE_FILE_SIZE, 317);
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
        // Entry didn't move — still LOAD_VADDR + 176 = 0x100B0 — but
        // the assertion guards future refactors.
        let entry = u64::from_le_bytes(INIT_FIXTURE_BYTES[24..32].try_into().unwrap());
        assert_eq!(entry, INIT_FIXTURE_ENTRY_VADDR);
        assert_eq!(entry, 0x100B0);
    }

    #[test]
    fn fixture_msg_bytes_present_for_child_and_parent() {
        // Both messages must round-trip through the byte stream. The
        // exact offsets are pinned by the auipc/addi immediates
        // documented in the module header; this test asserts the
        // bytes are present *somewhere* (defends against accidental
        // edits dropping or reordering the message section).
        assert!(
            INIT_FIXTURE_BYTES
                .windows(b"child\n".len())
                .any(|w| w == b"child\n"),
            "fixture must contain b\"child\\n\" somewhere",
        );
        assert!(
            INIT_FIXTURE_BYTES
                .windows(b"parent\n".len())
                .any(|w| w == b"parent\n"),
            "fixture must contain b\"parent\\n\" somewhere",
        );
        // Spot-check the documented offsets too — the auipc/addi
        // immediates depend on these positions.
        assert_eq!(&INIT_FIXTURE_BYTES[304..310], b"child\n");
        assert_eq!(&INIT_FIXTURE_BYTES[310..317], b"parent\n");
    }

    #[test]
    fn fixture_first_instruction_is_li_a7_220() {
        // The Wave 4 fixture leads with `li a7, 220` (NR_CLONE),
        // not the original `li a7, 64` (NR_WRITE) — the rename of
        // this test relative to the hello-world predecessor pins
        // the change explicitly.
        // Little-endian: 93 08 c0 0d → 0x0dc00893.
        let insn = u32::from_le_bytes(INIT_FIXTURE_BYTES[176..180].try_into().unwrap());
        assert_eq!(insn, 0x0dc00893);
    }

    #[test]
    fn fixture_load_vaddr_constants_consistent() {
        // PT_LOAD's p_vaddr at offset 136.
        let load_vaddr = u64::from_le_bytes(INIT_FIXTURE_BYTES[136..144].try_into().unwrap());
        assert_eq!(load_vaddr, INIT_FIXTURE_LOAD_VADDR);
        // entry = LOAD + 176.
        assert_eq!(INIT_FIXTURE_ENTRY_VADDR, INIT_FIXTURE_LOAD_VADDR + 176);
    }

    /// Decode the `bnez a0, parent_path` B-type instruction and
    /// assert it lands on the parent_path label. Defends against
    /// off-by-one in the immediate split (B-type's `imm[12|10:5|4:1|11]`
    /// shape is easy to mis-encode when hand-rolling).
    #[test]
    fn fixture_bnez_branch_offset_targets_parent_path() {
        let insn = u32::from_le_bytes(
            INIT_FIXTURE_BYTES[BNEZ_FILE_OFFSET..BNEZ_FILE_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        // Opcode + funct3 sanity: B-type with funct3 = BNE = 001.
        assert_eq!(insn & 0x7f, 0x63, "B-type opcode");
        assert_eq!((insn >> 12) & 0x7, 0b001, "funct3 == BNE");
        // rs1 = a0 (10), rs2 = x0 (0).
        assert_eq!((insn >> 15) & 0x1f, 10, "rs1 == a0");
        assert_eq!((insn >> 20) & 0x1f, 0, "rs2 == x0");

        // Decode the B-type immediate.
        // bit[31] = imm[12]; bits[30:25] = imm[10:5];
        // bits[11:8] = imm[4:1]; bit[7] = imm[11].
        let imm12 = (insn >> 31) & 0x1;
        let imm_10_5 = (insn >> 25) & 0x3f;
        let imm_4_1 = (insn >> 8) & 0xf;
        let imm11 = (insn >> 7) & 0x1;
        let mut offset = (imm_4_1 << 1) | (imm_10_5 << 5) | (imm11 << 11) | (imm12 << 12);
        // Sign-extend from bit 12.
        if imm12 != 0 {
            offset |= !0u32 << 13;
        }
        let offset = offset as i32;

        // Branch's PC = INIT_FIXTURE_ENTRY_VADDR + 28 (insn 7 of 32).
        let bnez_pc = INIT_FIXTURE_ENTRY_VADDR as i64 + 28;
        let parent_path_vaddr = INIT_FIXTURE_LOAD_VADDR + (PARENT_PATH_FILE_OFFSET as u64);
        assert_eq!(
            bnez_pc + offset as i64,
            parent_path_vaddr as i64,
            "bnez offset must land on parent_path (offset={offset}, bnez_pc=0x{bnez_pc:x}, \
             parent_path_vaddr=0x{parent_path_vaddr:x})",
        );
        // Sanity: also assert the offset itself equals the
        // documented +40-byte distance.
        assert_eq!(offset, 40, "documented bnez offset is +40 bytes");
    }
}
