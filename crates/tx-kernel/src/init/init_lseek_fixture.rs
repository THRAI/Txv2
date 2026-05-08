// Sibling `/lseek-target` fixture binary for the fd-ops slice
// (Wave 4, Part 6 + Part 8 — end-to-end smoke).
//
// ## Decision: sibling fixture, not extend-in-place
//
// `init_fixture.rs` is the fork+wait+exit smoke binary; its byte
// stream is pinned by ~7 host-side tests. Layering a third
// behaviour (openat → write → lseek → read → close → exit) on top
// of that already-load-bearing fixture would invalidate the
// existing pins and force a hand-rewrite of the fork/wait
// encoding. Following the DAC + setuid Wave 5 deviation
// (`init_setuid_fixture.rs`), this new fixture is a sibling — pure
// byte-pin shape, never wired into the bootstrap path. The
// meaningful coverage for `NR_LSEEK` lives in the tx-shims
// dispatch tests; this fixture pins the userspace ABI byte stream
// against accidental drift.
//
// ## Behaviour (intent — never executed end-to-end)
//
// ```text
// _start:
//     li   a7, 56        ; NR_OPENAT
//     ecall              ; openat(AT_FDCWD, path, O_RDWR|O_CREAT, 0644)
//     li   a7, 64        ; NR_WRITE
//     ecall              ; write(fd, "hello\n", 6)
//     li   a7, 62        ; NR_LSEEK
//     ecall              ; lseek(fd, 0, SEEK_SET)
//     li   a7, 63        ; NR_READ
//     ecall              ; read(fd, buf, 6)
//     li   a7, 57        ; NR_CLOSE
//     ecall              ; close(fd)
//     li   a7, 94        ; NR_EXIT_GROUP
//     li   a0, 0
//     ecall              ; exit_group(0)
// ```
//
// Argument GPRs (`a0..a6`) are intentionally left undefined. The
// Wave 4 brief is explicit that "Layer B" — actually running this
// through the reactor — is deferred per slice norm; the ABI byte
// stream is what matters here, not a working program.
//
// The Linux RV64 syscall ABI is: `a7` carries the syscall number,
// `a0..a6` the arguments, `ecall` traps. The shim layer's
// `linux_syscall::dispatch` matches on `a7` and dispatches to the
// corresponding arm. Wave 4 wired NR_LSEEK; the others shipped in
// earlier waves (`NR_OPENAT` / `NR_CLOSE` Wave 2; `NR_WRITE` /
// `NR_READ` / `NR_EXIT_GROUP` Phase 2a/b).
//
// ## ELF layout
//
// ```text
// Offset | Size | Contents
// -------|------|------------------------------------------------
//   0    |  64  | ELF64 Ehdr
//  64    |  56  | PT_PHDR (program-header self-cover)
// 120    |  56  | PT_LOAD (R+X, covers the entire file)
// 176    |  52  | code (13 RV64 instructions × 4 bytes)
// -------|------|------------------------------------------------
// Total: 228 bytes.
// ```
//
// `LOAD_VADDR = 0x10000` (low userspace; matches the fork/wait and
// setuid fixtures' load vaddr — the three fixtures are not
// co-resident in a single AddressSpace, so the shared vaddr is
// fine).
//
// Entry: `LOAD_VADDR + 176 = 0x100B0` (first instruction).
//
// ## RV64 byte map (each 32-bit instruction is little-endian)
//
// ```text
// Insn # | VAddr   | Hex Encoding | Mnemonic
// -------|---------|--------------|-------------------------------
//   0    | 0x100B0 | 93 08 80 03  | li   a7, 56        (0x03800893)
//   1    | 0x100B4 | 73 00 00 00  | ecall              (0x00000073)
//   2    | 0x100B8 | 93 08 00 04  | li   a7, 64        (0x04000893)
//   3    | 0x100BC | 73 00 00 00  | ecall              (0x00000073)
//   4    | 0x100C0 | 93 08 E0 03  | li   a7, 62        (0x03E00893)
//   5    | 0x100C4 | 73 00 00 00  | ecall              (0x00000073)
//   6    | 0x100C8 | 93 08 F0 03  | li   a7, 63        (0x03F00893)
//   7    | 0x100CC | 73 00 00 00  | ecall              (0x00000073)
//   8    | 0x100D0 | 93 08 90 03  | li   a7, 57        (0x03900893)
//   9    | 0x100D4 | 73 00 00 00  | ecall              (0x00000073)
//  10    | 0x100D8 | 93 08 E0 05  | li   a7, 94        (0x05E00893)
//  11    | 0x100DC | 13 05 00 00  | li   a0, 0         (0x00000513)
//  12    | 0x100E0 | 73 00 00 00  | ecall              (0x00000073)
// ```
//
// ## RV64 encoding cross-checks
//
// - `addi` (I-type): bits[31:20]=imm[11:0], bits[19:15]=rs1=0,
//   bits[14:12]=000 (funct3), bits[11:7]=rd, bits[6:0]=0010011.
//   `li rd, k` is `addi rd, x0, k`.
// - `li a7, 56`: rd=17 (a7), imm=56=0x038 → `0x03800893`.
// - `li a7, 64`: rd=17, imm=64=0x040 → `0x04000893`.
// - `li a7, 62`: rd=17, imm=62=0x03E → `0x03E00893`.
// - `li a7, 63`: rd=17, imm=63=0x03F → `0x03F00893`.
// - `li a7, 57`: rd=17, imm=57=0x039 → `0x03900893`.
// - `li a7, 94`: rd=17, imm=94=0x05E → `0x05E00893`.
// - `li a0, 0`:  rd=10, imm=0 → `0x00000513`.
// - `ecall` is the all-zeros encoding plus opcode `0x73`
//   (`0x00000073`).
// - All immediates are within ±2047 so no `lui` prefix is needed.

/// LOAD virtual address (entry of the PT_LOAD segment).
#[allow(dead_code)] // referenced only from the host-side test below.
pub const INIT_LSEEK_FIXTURE_LOAD_VADDR: u64 = 0x10000;

/// Entry-point virtual address (first instruction).
/// Equal to LOAD_VADDR + 176 (header + 2 PHDRs).
#[allow(dead_code)] // referenced only from the host-side test below.
pub const INIT_LSEEK_FIXTURE_ENTRY_VADDR: u64 = INIT_LSEEK_FIXTURE_LOAD_VADDR + 176;

/// Total fixture size in bytes (also `p_filesz` and `p_memsz` of
/// the PT_LOAD segment).
pub const INIT_LSEEK_FIXTURE_FILE_SIZE: usize = 228;

/// Hand-encoded RV64 ET_EXEC ELF binary. See module docstring for
/// the layout, byte map, and disassembly.
///
/// Stored as a top-level `static` so any future end-to-end smoke
/// (Layer B) can take `&[u8]` references into the kernel image's
/// `.rodata`.
#[rustfmt::skip]
pub static INIT_LSEEK_FIXTURE_BYTES: [u8; INIT_LSEEK_FIXTURE_FILE_SIZE] = {
    let mut bytes = [0u8; INIT_LSEEK_FIXTURE_FILE_SIZE];

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
    // e_entry = INIT_LSEEK_FIXTURE_ENTRY_VADDR (offset 24, u64 LE).
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
    // p_filesz = INIT_LSEEK_FIXTURE_FILE_SIZE = 228 (offset 152, u64 LE).
    // 228 = 0xE4 → bytes E4 00 00 00 00 00 00 00.
    bytes[152] = 0xE4;
    // p_memsz = same (offset 160)
    bytes[160] = 0xE4;
    // p_align = 0x1000 (offset 168)
    bytes[168] = 0x00; bytes[169] = 0x10; bytes[170] = 0x00; bytes[171] = 0x00;

    // ----- Code (offset 176..228) -------------------------------
    //
    // li a7, 56 (NR_OPENAT) → 0x03800893
    bytes[176] = 0x93; bytes[177] = 0x08; bytes[178] = 0x80; bytes[179] = 0x03;
    // ecall → 0x00000073
    bytes[180] = 0x73; bytes[181] = 0x00; bytes[182] = 0x00; bytes[183] = 0x00;
    // li a7, 64 (NR_WRITE) → 0x04000893
    bytes[184] = 0x93; bytes[185] = 0x08; bytes[186] = 0x00; bytes[187] = 0x04;
    // ecall → 0x00000073
    bytes[188] = 0x73; bytes[189] = 0x00; bytes[190] = 0x00; bytes[191] = 0x00;
    // li a7, 62 (NR_LSEEK) → 0x03E00893
    bytes[192] = 0x93; bytes[193] = 0x08; bytes[194] = 0xE0; bytes[195] = 0x03;
    // ecall → 0x00000073
    bytes[196] = 0x73; bytes[197] = 0x00; bytes[198] = 0x00; bytes[199] = 0x00;
    // li a7, 63 (NR_READ) → 0x03F00893
    bytes[200] = 0x93; bytes[201] = 0x08; bytes[202] = 0xF0; bytes[203] = 0x03;
    // ecall → 0x00000073
    bytes[204] = 0x73; bytes[205] = 0x00; bytes[206] = 0x00; bytes[207] = 0x00;
    // li a7, 57 (NR_CLOSE) → 0x03900893
    bytes[208] = 0x93; bytes[209] = 0x08; bytes[210] = 0x90; bytes[211] = 0x03;
    // ecall → 0x00000073
    bytes[212] = 0x73; bytes[213] = 0x00; bytes[214] = 0x00; bytes[215] = 0x00;
    // li a7, 94 (NR_EXIT_GROUP) → 0x05E00893
    bytes[216] = 0x93; bytes[217] = 0x08; bytes[218] = 0xE0; bytes[219] = 0x05;
    // li a0, 0 → 0x00000513
    bytes[220] = 0x13; bytes[221] = 0x05; bytes[222] = 0x00; bytes[223] = 0x00;
    // ecall → 0x00000073
    bytes[224] = 0x73; bytes[225] = 0x00; bytes[226] = 0x00; bytes[227] = 0x00;

    bytes
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: the byte buffer's length matches the published constant
    /// (and the documented 228 bytes). Defends against accidental
    /// edits silently shrinking or growing the fixture.
    #[test]
    fn lseek_fixture_size_matches_constant() {
        assert_eq!(
            INIT_LSEEK_FIXTURE_BYTES.len(),
            INIT_LSEEK_FIXTURE_FILE_SIZE
        );
        assert_eq!(INIT_LSEEK_FIXTURE_FILE_SIZE, 228);
    }

    /// Pin: ELF magic + ELFCLASS64 + ELFDATA2LSB header bytes are
    /// in place. Without these, the ELF parser rejects the fixture
    /// pre-Phase-3.
    #[test]
    fn lseek_fixture_starts_with_elf_magic() {
        assert_eq!(&INIT_LSEEK_FIXTURE_BYTES[0..4], b"\x7fELF");
        assert_eq!(INIT_LSEEK_FIXTURE_BYTES[4], 2); // ELFCLASS64
        assert_eq!(INIT_LSEEK_FIXTURE_BYTES[5], 1); // ELFDATA2LSB
    }

    /// Pin: e_machine encodes EM_RISCV (243 = 0xf3). The exec
    /// parser requires this to match the platform.
    #[test]
    fn lseek_fixture_e_machine_is_riscv() {
        // EM_RISCV = 243 = 0xf3.
        assert_eq!(INIT_LSEEK_FIXTURE_BYTES[18], 0xf3);
        assert_eq!(INIT_LSEEK_FIXTURE_BYTES[19], 0x00);
    }

    /// Pin: e_entry equals the documented entry-vaddr constant
    /// (`LOAD_VADDR + 176 = 0x100B0`). Any future Layer B smoke
    /// asserts on the post-exec `saved_user_context.pc` matching
    /// this value, so a drift here would silently break the smoke.
    #[test]
    fn lseek_fixture_e_entry_matches_constant() {
        let entry = u64::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[24..32].try_into().unwrap());
        assert_eq!(entry, INIT_LSEEK_FIXTURE_ENTRY_VADDR);
        assert_eq!(entry, 0x100B0);
    }

    /// Pin: the first instruction at the entry vaddr encodes
    /// `li a7, 56` (NR_OPENAT = 56 = 0x03800893). This is the
    /// distinguishing assertion vs the fork/wait fixture (which
    /// leads with `li a7, 220`, NR_CLONE) and the setuid fixture
    /// (which leads with `li a7, 174`, NR_GETUID) — even if all
    /// fixtures shared an entry vaddr, the byte stream pins them
    /// apart.
    ///
    /// Mirrors the fork/wait smoke
    /// (`boot_smoke_fork_wait_seeds_init_for_clone_at_entry`)'s
    /// distinguishing-instruction assertion. Wave 4 of the fd-ops
    /// slice ships only Layer A (this byte-pin smoke) per the
    /// brief; Layer B (reactor-driven end-to-end) is deferred.
    #[test]
    fn lseek_fixture_first_instruction_is_li_a7_56_nr_openat() {
        // Little-endian: 93 08 80 03 → 0x03800893.
        let insn = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[176..180].try_into().unwrap());
        assert_eq!(insn, 0x03800893);
    }

    /// Pin: the syscall-number sequence matches the planned shape
    /// (openat → write → lseek → read → close → exit_group). Each
    /// `li a7, NR` is at instruction index `2*k` for k ∈ 0..6
    /// (instruction 12 is the trailing `li a0, 0`).
    #[test]
    fn lseek_fixture_syscall_sequence_pins_planned_shape() {
        // Insn 0 (offset 176): NR_OPENAT (56)
        let i0 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[176..180].try_into().unwrap());
        assert_eq!(i0, 0x03800893, "insn 0 should be `li a7, 56` (NR_OPENAT)");
        // Insn 2 (offset 184): NR_WRITE (64)
        let i2 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[184..188].try_into().unwrap());
        assert_eq!(i2, 0x04000893, "insn 2 should be `li a7, 64` (NR_WRITE)");
        // Insn 4 (offset 192): NR_LSEEK (62) — Wave 4's load-bearing arm.
        let i4 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[192..196].try_into().unwrap());
        assert_eq!(i4, 0x03E00893, "insn 4 should be `li a7, 62` (NR_LSEEK)");
        // Insn 6 (offset 200): NR_READ (63)
        let i6 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[200..204].try_into().unwrap());
        assert_eq!(i6, 0x03F00893, "insn 6 should be `li a7, 63` (NR_READ)");
        // Insn 8 (offset 208): NR_CLOSE (57)
        let i8 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[208..212].try_into().unwrap());
        assert_eq!(i8, 0x03900893, "insn 8 should be `li a7, 57` (NR_CLOSE)");
        // Insn 10 (offset 216): NR_EXIT_GROUP (94)
        let i10 = u32::from_le_bytes(INIT_LSEEK_FIXTURE_BYTES[216..220].try_into().unwrap());
        assert_eq!(i10, 0x05E00893, "insn 10 should be `li a7, 94` (NR_EXIT_GROUP)");
    }
}
