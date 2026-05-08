// Sibling `/setuid-target` fixture binary for the DAC + setuid slice
// (Wave 5, Part 8 — end-to-end smoke).
//
// ## Decision: sibling fixture, not extend-in-place
//
// The plan's Open Q #4 (DECIDED 2026-05-06) authored before the
// fork/clone/wait4 fixture rewrite recommended *extending*
// `init_fixture.rs` to ALSO carry the setuid switcher shape. Since
// that decision was made, the fork/clone/wait4 slice's Wave 4 grew
// `init_fixture.rs` into a fork+wait+exit binary with ~7 pinned
// byte tests; layering a third behaviour (drop-privs → execve →
// observe-euid) on top of that already-load-bearing fixture would
// require another rewrite of the whole hand-encoding without
// benefiting either smoke. **Wave 5 deviation: ship the sibling
// fixture (`init_setuid_fixture.rs`) the plan's Part 8 section
// heading already named.** The fork/wait fixture stays a stable
// smoke target; this new fixture is purely a setuid drop-target.
//
// ## Behaviour
//
// ```text
// _start:
//     li   a7, 174       ; NR_GETUID
//     ecall              ; a0 ← real uid
//     li   a7, 175       ; NR_GETEUID
//     ecall              ; a0 ← effective uid
//     li   a7, 94        ; NR_EXIT_GROUP
//     li   a0, 0
//     ecall
// ```
//
// Pure cred-introspection + clean exit. No `write` to the console —
// the smoke asserts on cred state and `saved_user_context.pc`,
// not on console output. Compared to the fork/clone/wait4
// fixture's 32 instructions, this fixture is 7 instructions
// (28 bytes of code).
//
// The Linux RV64 syscall ABI is: `a7` carries the syscall number,
// `a0..a6` the arguments, `ecall` traps. The shim layer's
// `linux_syscall::dispatch` matches on `a7` and dispatches to the
// corresponding arm. Wave 2 of this slice wired NR_GETUID /
// NR_GETEUID. NR_EXIT_GROUP shipped pre-slice via the ELF loader.
//
// ## ELF layout
//
// ```text
// Offset | Size | Contents
// -------|------|------------------------------------------------
//   0    |  64  | ELF64 Ehdr
//  64    |  56  | PT_PHDR (program-header self-cover)
// 120    |  56  | PT_LOAD (R+X, covers the entire file)
// 176    |  28  | code (7 RV64 instructions × 4 bytes)
// -------|------|------------------------------------------------
// Total: 204 bytes.
// ```
//
// `LOAD_VADDR = 0x10000` (low userspace; matches the fork/wait
// fixture's load vaddr — the two fixtures are not co-resident in
// a single AddressSpace, so the shared vaddr is fine).
//
// Entry: `LOAD_VADDR + 176 = 0x100B0` (first instruction).
//
// ## RV64 byte map (each 32-bit instruction is little-endian)
//
// ```text
// Insn # | VAddr   | Hex Encoding | Mnemonic
// -------|---------|--------------|-------------------------------
//   0    | 0x100B0 | 93 08 E0 0A  | li   a7, 174       (0x0AE00893)
//   1    | 0x100B4 | 73 00 00 00  | ecall              (0x00000073)
//   2    | 0x100B8 | 93 08 F0 0A  | li   a7, 175       (0x0AF00893)
//   3    | 0x100BC | 73 00 00 00  | ecall              (0x00000073)
//   4    | 0x100C0 | 93 08 E0 05  | li   a7, 94        (0x05E00893)
//   5    | 0x100C4 | 13 05 00 00  | li   a0, 0         (0x00000513)
//   6    | 0x100C8 | 73 00 00 00  | ecall              (0x00000073)
// ```
//
// ## RV64 encoding cross-checks
//
// - `addi` (I-type): bits[31:20]=imm[11:0], bits[19:15]=rs1=0,
//   bits[14:12]=000 (funct3), bits[11:7]=rd, bits[6:0]=0010011.
//   `li rd, k` is `addi rd, x0, k`.
// - `li a7, 174`: rd=17 (a7), imm=174=0xAE → `0AE00893`.
// - `li a7, 175`: rd=17, imm=175=0xAF → `0AF00893`.
// - `li a7, 94`:  rd=17, imm=94=0x5E → `05E00893`.
// - `li a0, 0`:   rd=10, imm=0 → `00000513`.
// - `ecall` is the all-zeros encoding plus opcode `0x73`
//   (`0x00000073`).
// - All immediates are within ±2047 so no `lui` prefix is needed.
//
// (Encodings cross-checked against RV64I ABI / Volume I:
// User-Level ISA §2.5 / §2.7.)

/// LOAD virtual address (entry of the PT_LOAD segment).
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_SETUID_FIXTURE_LOAD_VADDR: u64 = 0x10000;

/// Entry-point virtual address (first instruction).
/// Equal to LOAD_VADDR + 176 (header + 2 PHDRs).
#[allow(dead_code)] // referenced from host-side tests (#[cfg(test)]).
pub const INIT_SETUID_FIXTURE_ENTRY_VADDR: u64 = INIT_SETUID_FIXTURE_LOAD_VADDR + 176;

/// Total fixture size in bytes (also `p_filesz` and `p_memsz` of the
/// PT_LOAD segment).
pub const INIT_SETUID_FIXTURE_FILE_SIZE: usize = 204;

/// Hand-encoded RV64 ET_EXEC ELF binary. See module docstring for
/// the layout, byte map, and disassembly.
///
/// Stored as a top-level `static` so consumers (the smoke's
/// `register_setuid_fixture_into_tmpfs` helper) can take `&[u8]`
/// references into the kernel image's `.rodata`.
#[rustfmt::skip]
pub static INIT_SETUID_FIXTURE_BYTES: [u8; INIT_SETUID_FIXTURE_FILE_SIZE] = {
    let mut bytes = [0u8; INIT_SETUID_FIXTURE_FILE_SIZE];

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
    // e_entry = INIT_SETUID_FIXTURE_ENTRY_VADDR (offset 24, u64 LE).
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
    // p_filesz = INIT_SETUID_FIXTURE_FILE_SIZE = 204 (offset 152, u64 LE).
    // 204 = 0xCC → bytes CC 00 00 00 00 00 00 00.
    bytes[152] = 0xCC;
    // p_memsz = same (offset 160)
    bytes[160] = 0xCC;
    // p_align = 0x1000 (offset 168)
    bytes[168] = 0x00; bytes[169] = 0x10; bytes[170] = 0x00; bytes[171] = 0x00;

    // ----- Code (offset 176..204) -------------------------------
    //
    // li a7, 174 (NR_GETUID) → 0x0AE00893
    bytes[176] = 0x93; bytes[177] = 0x08; bytes[178] = 0xE0; bytes[179] = 0x0A;
    // ecall → 0x00000073
    bytes[180] = 0x73; bytes[181] = 0x00; bytes[182] = 0x00; bytes[183] = 0x00;
    // li a7, 175 (NR_GETEUID) → 0x0AF00893
    bytes[184] = 0x93; bytes[185] = 0x08; bytes[186] = 0xF0; bytes[187] = 0x0A;
    // ecall → 0x00000073
    bytes[188] = 0x73; bytes[189] = 0x00; bytes[190] = 0x00; bytes[191] = 0x00;
    // li a7, 94 (NR_EXIT_GROUP) → 0x05E00893
    bytes[192] = 0x93; bytes[193] = 0x08; bytes[194] = 0xE0; bytes[195] = 0x05;
    // li a0, 0 → 0x00000513
    bytes[196] = 0x13; bytes[197] = 0x05; bytes[198] = 0x00; bytes[199] = 0x00;
    // ecall → 0x00000073
    bytes[200] = 0x73; bytes[201] = 0x00; bytes[202] = 0x00; bytes[203] = 0x00;

    bytes
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: the byte buffer's length matches the published constant
    /// (and the documented 204 bytes). Defends against accidental
    /// edits silently shrinking or growing the fixture.
    #[test]
    fn setuid_fixture_size_matches_constant() {
        assert_eq!(
            INIT_SETUID_FIXTURE_BYTES.len(),
            INIT_SETUID_FIXTURE_FILE_SIZE
        );
        assert_eq!(INIT_SETUID_FIXTURE_FILE_SIZE, 204);
    }

    /// Pin: ELF magic + ELFCLASS64 + ELFDATA2LSB header bytes are
    /// in place. Without these, the parser rejects the fixture
    /// pre-Phase-3.
    #[test]
    fn setuid_fixture_starts_with_elf_magic() {
        assert_eq!(&INIT_SETUID_FIXTURE_BYTES[0..4], b"\x7fELF");
        assert_eq!(INIT_SETUID_FIXTURE_BYTES[4], 2); // ELFCLASS64
        assert_eq!(INIT_SETUID_FIXTURE_BYTES[5], 1); // ELFDATA2LSB
    }

    /// Pin: e_machine encodes EM_RISCV (243 = 0xf3). The exec
    /// parser requires this to match the platform.
    #[test]
    fn setuid_fixture_e_machine_is_riscv() {
        // EM_RISCV = 243 = 0xf3.
        assert_eq!(INIT_SETUID_FIXTURE_BYTES[18], 0xf3);
        assert_eq!(INIT_SETUID_FIXTURE_BYTES[19], 0x00);
    }

    /// Pin: e_entry equals the documented entry-vaddr constant
    /// (`LOAD_VADDR + 176 = 0x100B0`). The smoke asserts the
    /// post-exec `saved_user_context.pc` matches this value, so a
    /// drift here would silently break the smoke.
    #[test]
    fn setuid_fixture_e_entry_matches_constant() {
        let entry = u64::from_le_bytes(INIT_SETUID_FIXTURE_BYTES[24..32].try_into().unwrap());
        assert_eq!(entry, INIT_SETUID_FIXTURE_ENTRY_VADDR);
        assert_eq!(entry, 0x100B0);
    }

    /// Pin: the first instruction at the entry vaddr encodes
    /// `li a7, 174` (NR_GETUID = 174 = 0x0AE00893). This is the
    /// distinguishing assertion vs the fork/wait fixture (which
    /// leads with `li a7, 220`, NR_CLONE) — even if both fixtures
    /// shared an entry vaddr, the byte stream pins them apart.
    #[test]
    fn setuid_fixture_first_instruction_is_li_a7_174() {
        // Little-endian: 93 08 E0 0A → 0x0AE00893.
        let insn = u32::from_le_bytes(INIT_SETUID_FIXTURE_BYTES[176..180].try_into().unwrap());
        assert_eq!(insn, 0x0AE00893);
    }
}
