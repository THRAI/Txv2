//! Initial userspace stack image builder for the exec script
//! (Phase 4 / §9.3 of `EXEC_v1`).
//!
//! Doc anchors:
//!   txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK
//!   txdoc:EXEC-9-4-AUXV-CONSTRUCTION
//!
//! Companion research:
//!   docs/progress/research/2026-05-06-musl-ltp-execve-coverage.md
//!   ("musl static-link minimum kernel surface")
//!
//! This module is a *pure* kernel-side function: it composes the
//! initial userspace stack into a `Vec<u8>` (no device I/O, no
//! allocation tied to a particular AddressSpace). Sibling Phase 1A
//! ships `vm::populate_detached_user_range`, which writes the image
//! into a freshly-built detached `Cap<AddressSpace>`. Sibling Phase 4
//! produces the parsed `ExecImagePlan` whose `phdr_va` / `phent` /
//! `phnum` feed `AuxvFacts`.
//!
//! The musl static path reads the stack at `_start` per the System V
//! psABI for RV64 (musl `crt/crt1.c` and `src/env/__libc_start_main.c`
//! and `src/env/__init_tls.c`). v1 emits exactly the six entries
//! musl actually requires: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`,
//! `AT_PAGESZ`, `AT_RANDOM`, `AT_NULL`.

use alloc::vec;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Auxv `a_type` constants. Linux RV64 generic ABI; the values are stable
// across architectures (defined in <elf.h> / <linux/auxvec.h>).
//
// TODO(phase-goblin-share): once Phase 4 lands the goblin dep, share these
// with `goblin::elf::auxv` instead of hardcoding. The numeric values are
// frozen by the Linux uABI, so the hardcoded constants here are
// definitionally identical.
// ---------------------------------------------------------------------------

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_RANDOM: u64 = 25;

/// Auxv pair size in bytes (`a_type: u64, a_val: u64`).
const AUXV_PAIR_SIZE: usize = 16;

/// Width of a single pointer / counter on RV64.
const WORD_SIZE: usize = 8;

/// AT_RANDOM region size (musl reads exactly 16 bytes via
/// `__init_ssp((void*)aux[AT_RANDOM])`).
const AT_RANDOM_REGION_SIZE: usize = 16;

/// SysV RV64 psABI mandates 16-byte stack alignment at `_start`.
const STACK_ALIGN: usize = 16;

/// Number of auxv pairs we emit (5 facts + AT_NULL terminator).
const AUXV_PAIR_COUNT: usize = 6;

/// Composed stack image ready to write into a detached `AddressSpace`.
pub struct UserStackImage {
    /// Initial user-space stack pointer (16-byte aligned per RV64
    /// SysV psABI). Points at the `argc` word.
    pub initial_sp: u64,
    /// Stack bytes to install at addresses
    /// `[initial_sp, initial_sp + bytes.len())` in the detached AS.
    /// Includes the argc / argv-ptrs / envp-ptrs / auxv table, the
    /// `AT_RANDOM` region, the argv/envp string pool, and any
    /// alignment padding.
    pub bytes: Vec<u8>,
}

/// Auxiliary-vector facts the loader knows after parsing the ELF.
///
/// (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`) come from the ELF header and
/// the `PT_PHDR` phdr (or the loader's fallback computation per
/// `txdoc:EXEC-8-6-AT-PHDR-COMPUTATION`); `AT_PAGESZ` is the
/// platform's user page size (4096 on RV64 today).
pub struct AuxvFacts {
    /// Virtual address of the program-header table (after load bias,
    /// which is zero for static-`ET_EXEC` v1).
    pub at_phdr: u64,
    /// Size of one program header. Always 56 on RV64 ELF64.
    pub at_phent: u64,
    /// Count of program headers.
    pub at_phnum: u64,
    /// Page size. 4096 on RV64.
    pub at_pagesz: u64,
}

/// Build the initial userspace stack image for execve.
///
/// `stack_top` is the highest user-VA byte the stack image occupies;
/// the layout grows *down* from there. The function computes the
/// resulting `initial_sp` (16-byte aligned) and returns the
/// kernel-side bytes to write.
///
/// `argv` and `envp` are byte slices per element (NUL terminator is
/// appended by this function — the caller passes the bytes *without*
/// the trailing NUL). The function copies the strings into the
/// stack-image string pool.
///
/// CVE-2021-4034 mitigation: if `argv` is empty, synthesise a single
/// `argv[0] = b""` so userspace never observes `argc=0` with a NULL
/// `argv[0]` (the polkit pwnkit shape). Cross-doc anchor:
/// `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS` "kernel synthesises a dummy
/// argv[0] when caller passes empty argv".
///
/// Cites: `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`;
/// musl `crt/crt1.c`, `src/env/__libc_start_main.c`,
/// `src/env/__init_tls.c`.
///
/// # Layout (high → low addresses, `stack_top` at the very top)
///
/// ```text
///   [high addresses]
///   ┌──────────────────────────────────┐  stack_top
///   │   string pool (envp strings)     │
///   │   string pool (argv strings)     │
///   │   16-byte AT_RANDOM region       │
///   │   (16-byte alignment padding)    │
///   ├──────────────────────────────────┤
///   │   auxv terminator: AT_NULL = 0   │
///   │   auxv: AT_RANDOM ptr -> region  │
///   │   auxv: AT_PAGESZ                │
///   │   auxv: AT_PHNUM                 │
///   │   auxv: AT_PHENT                 │
///   │   auxv: AT_PHDR                  │
///   ├──────────────────────────────────┤
///   │   envp terminator: NULL          │
///   │   envp[N-1] ptr                  │
///   │   ...                            │
///   │   envp[0] ptr                    │
///   ├──────────────────────────────────┤
///   │   argv terminator: NULL          │
///   │   argv[argc-1] ptr               │
///   │   ...                            │
///   │   argv[0] ptr                    │
///   ├──────────────────────────────────┤
///   │   argc (u64)                     │  initial_sp -> here
///   └──────────────────────────────────┘
///   [low addresses]
/// ```
pub fn build_initial_user_stack(
    stack_top: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    auxv_facts: &AuxvFacts,
) -> UserStackImage {
    // ---- 1. CVE-2021-4034 dummy argv[0] synthesis ------------------------
    //
    // The synthetic empty argv[0] keeps userspace away from the
    // "argc=0 + NULL argv[0]" trap. We copy the synthesised slice
    // into a local Vec so the borrow lives long enough to feed the
    // string pool below.
    let dummy_argv: [&[u8]; 1] = [b""];
    let argv_view: &[&[u8]] = if argv.is_empty() { &dummy_argv } else { argv };
    let argc = argv_view.len();
    let envc = envp.len();

    // ---- 2. Compute string-pool size (argv strings + envp strings) ------
    //
    // Each string lands in the pool with a trailing NUL byte. We do
    // not deduplicate.
    let mut argv_string_total: usize = 0;
    for s in argv_view {
        argv_string_total += s.len() + 1;
    }
    let mut envp_string_total: usize = 0;
    for s in envp {
        envp_string_total += s.len() + 1;
    }
    let string_pool_size = argv_string_total + envp_string_total;

    // ---- 3. Compute the upper-table size --------------------------------
    //
    //   argc (8) + (argc+1) argv ptrs + (envc+1) envp ptrs + 6 auxv pairs
    //
    // The auxv pair count is fixed at 6 (5 facts + AT_NULL).
    let upper_table_size = WORD_SIZE                   // argc
        + (argc + 1) * WORD_SIZE                       // argv ptrs + NULL
        + (envc + 1) * WORD_SIZE                       // envp ptrs + NULL
        + AUXV_PAIR_COUNT * AUXV_PAIR_SIZE; // auxv

    // ---- 4. Compute total bytes and 16-byte alignment padding ------------
    //
    // Layout (high → low):
    //   string_pool    (envp strings then argv strings)
    //   AT_RANDOM      (16 bytes, also 16-byte aligned)
    //   pad            (so initial_sp = stack_top - total ends up 16-aligned)
    //   upper_table    (argc + ptrs + auxv)
    //
    // We want `initial_sp = stack_top - total` to be 16-byte aligned.
    // `stack_top` itself is required by callers to be page-aligned
    // (and therefore 16-byte aligned) — but we do not lean on that
    // assumption: we compute padding from `stack_top` modulo 16
    // explicitly.
    let unpadded = upper_table_size + AT_RANDOM_REGION_SIZE + string_pool_size;
    let stack_top_mod = (stack_top as usize) & (STACK_ALIGN - 1);
    // We need (stack_top - total) mod 16 == 0
    //   <=> total mod 16 == stack_top mod 16
    let need = stack_top_mod;
    let have = unpadded & (STACK_ALIGN - 1);
    let pad = (need + STACK_ALIGN - have) & (STACK_ALIGN - 1);
    let total = unpadded + pad;

    let initial_sp = stack_top - total as u64;
    debug_assert!(initial_sp % STACK_ALIGN as u64 == 0);

    // ---- 5. Compute key user-VA addresses --------------------------------
    //
    // String-pool layout (low → high addresses):
    //   [argv_pool_base, argv_pool_base + argv_string_total)
    //   [envp_pool_base, envp_pool_base + envp_string_total)
    //
    // Sitting just below the AT_RANDOM region, which sits at
    //   [stack_top - AT_RANDOM_REGION_SIZE, stack_top).
    //
    // Wait — the layout block above places the string pool *between*
    // AT_RANDOM and stack_top:
    //
    //     stack_top
    //     [envp strings]
    //     [argv strings]
    //     [AT_RANDOM 16B]
    //     [pad]
    //     [upper_table]
    //
    // But musl reads AT_RANDOM via a pointer; its physical position
    // in the pool is irrelevant as long as the auxv entry's a_val
    // points at 16 contiguous bytes. We pin the layout above for
    // determinism. The string pool occupies the topmost bytes; the
    // AT_RANDOM region sits just below the argv strings (i.e. between
    // pad and argv_pool_base).
    //
    // Concretely:
    //   envp_pool_top  = stack_top
    //   envp_pool_base = envp_pool_top - envp_string_total
    //   argv_pool_top  = envp_pool_base
    //   argv_pool_base = argv_pool_top - argv_string_total
    //   at_random_top  = argv_pool_base
    //   at_random_base = at_random_top - AT_RANDOM_REGION_SIZE
    //
    // upper_table_top    = at_random_base - pad
    // upper_table_base   = upper_table_top - upper_table_size
    //                    = initial_sp
    let envp_pool_top = stack_top;
    let envp_pool_base = envp_pool_top - envp_string_total as u64;
    let argv_pool_top = envp_pool_base;
    let argv_pool_base = argv_pool_top - argv_string_total as u64;
    let at_random_top = argv_pool_base;
    let at_random_base = at_random_top - AT_RANDOM_REGION_SIZE as u64;

    // ---- 6. Allocate the kernel buffer ----------------------------------
    //
    // `bytes[0]` corresponds to `initial_sp` (the lowest byte we
    // emit). `bytes[total - 1]` corresponds to `stack_top - 1`.
    let mut bytes: Vec<u8> = vec![0u8; total];

    // Helper: write a u64 (little-endian) at offset `off` in `bytes`.
    // RV64 is little-endian; psABI fixes `Elf64_addr`/`Elf64_xword`
    // as LE on RISC-V.
    fn write_u64(bytes: &mut [u8], off: usize, v: u64) {
        bytes[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    // ---- 7. Write the upper table (argc, argv ptrs, envp ptrs, auxv) ----
    //
    // Offsets are relative to `initial_sp` == bytes[0].
    let mut off: usize = 0;

    // argc
    write_u64(&mut bytes, off, argc as u64);
    off += WORD_SIZE;

    // argv pointers — point into the string pool; layout the strings
    // densely starting at `argv_pool_base`.
    let mut argv_string_cursor = argv_pool_base;
    for s in argv_view {
        write_u64(&mut bytes, off, argv_string_cursor);
        off += WORD_SIZE;
        argv_string_cursor += (s.len() + 1) as u64;
    }
    // argv NULL terminator
    write_u64(&mut bytes, off, 0);
    off += WORD_SIZE;
    debug_assert!(argv_string_cursor == argv_pool_top);

    // envp pointers
    let mut envp_string_cursor = envp_pool_base;
    for s in envp {
        write_u64(&mut bytes, off, envp_string_cursor);
        off += WORD_SIZE;
        envp_string_cursor += (s.len() + 1) as u64;
    }
    // envp NULL terminator
    write_u64(&mut bytes, off, 0);
    off += WORD_SIZE;
    debug_assert!(envp_string_cursor == envp_pool_top);

    // auxv (fixed order: AT_PHDR, AT_PHENT, AT_PHNUM, AT_PAGESZ,
    // AT_RANDOM, AT_NULL).
    let auxv_entries: [(u64, u64); AUXV_PAIR_COUNT] = [
        (AT_PHDR, auxv_facts.at_phdr),
        (AT_PHENT, auxv_facts.at_phent),
        (AT_PHNUM, auxv_facts.at_phnum),
        (AT_PAGESZ, auxv_facts.at_pagesz),
        (AT_RANDOM, at_random_base),
        (AT_NULL, 0),
    ];
    for (a_type, a_val) in auxv_entries {
        write_u64(&mut bytes, off, a_type);
        write_u64(&mut bytes, off + WORD_SIZE, a_val);
        off += AUXV_PAIR_SIZE;
    }
    debug_assert!(off == upper_table_size);

    // ---- 8. Padding region is already zeroed by `vec![0u8; total]` ------
    //
    // Skip past pad bytes.
    off += pad;
    debug_assert!(off == upper_table_size + pad);

    // ---- 9. AT_RANDOM region: 16 bytes of constant zero ----------------
    //
    // TODO(phase-csprng): replace with bytes from a real CSPRNG once
    // the entropy subsystem ships. v1 accepts the SSP-canary
    // weakness per Open Q #1 DECIDED 2026-05-06; txKernel has no
    // ASLR and no stack-canary checks at this stage.
    let at_random_off = upper_table_size + pad;
    debug_assert!(at_random_off + AT_RANDOM_REGION_SIZE <= total);
    // bytes[at_random_off..at_random_off+16] is already zero.

    // ---- 10. argv string pool ------------------------------------------
    //
    // Lives at user-VA `argv_pool_base`, which corresponds to byte
    // offset `argv_pool_base - initial_sp`.
    let argv_pool_off = (argv_pool_base - initial_sp) as usize;
    let mut cursor = argv_pool_off;
    for s in argv_view {
        bytes[cursor..cursor + s.len()].copy_from_slice(s);
        cursor += s.len();
        bytes[cursor] = 0; // NUL terminator
        cursor += 1;
    }

    // ---- 11. envp string pool ------------------------------------------
    let envp_pool_off = (envp_pool_base - initial_sp) as usize;
    let mut cursor = envp_pool_off;
    for s in envp {
        bytes[cursor..cursor + s.len()].copy_from_slice(s);
        cursor += s.len();
        bytes[cursor] = 0;
        cursor += 1;
    }

    UserStackImage { initial_sp, bytes }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Compose a default `AuxvFacts` whose values are easy to spot in
    /// hex dumps when debugging a layout regression.
    fn facts() -> AuxvFacts {
        AuxvFacts {
            at_phdr: 0x4000_0040,
            at_phent: 56,
            at_phnum: 7,
            at_pagesz: 4096,
        }
    }

    /// Read a u64 little-endian from `bytes` at offset `off`.
    fn read_u64(bytes: &[u8], off: usize) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[off..off + 8]);
        u64::from_le_bytes(buf)
    }

    /// Translate an absolute user-VA pointer pulled out of the stack
    /// image back to a kernel-side byte offset within `image.bytes`.
    fn ptr_to_off(image: &UserStackImage, ptr: u64) -> usize {
        assert!(
            ptr >= image.initial_sp,
            "ptr {:#x} below sp {:#x}",
            ptr,
            image.initial_sp
        );
        let off = (ptr - image.initial_sp) as usize;
        assert!(off < image.bytes.len(), "ptr {:#x} past stack image", ptr);
        off
    }

    /// Read a NUL-terminated string from the stack image starting at
    /// the byte offset corresponding to user-VA `ptr`.
    fn read_cstring(image: &UserStackImage, ptr: u64) -> Vec<u8> {
        let mut off = ptr_to_off(image, ptr);
        let mut out = Vec::new();
        while image.bytes[off] != 0 {
            out.push(image.bytes[off]);
            off += 1;
        }
        out
    }

    #[test]
    fn build_initial_user_stack_zero_argv_zero_envp_minimum_layout() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts());

        // sp is 16-byte aligned per psABI.
        assert_eq!(image.initial_sp & 0xF, 0);
        assert!(image.initial_sp < stack_top);
        assert_eq!(image.initial_sp + image.bytes.len() as u64, stack_top);

        // argc == 1 (CVE-2021-4034 synthetic argv[0]).
        let argc = read_u64(&image.bytes, 0);
        assert_eq!(argc, 1, "synthetic dummy argv[0] should make argc=1");

        // argv[0] points at a NUL byte.
        let argv0 = read_u64(&image.bytes, WORD_SIZE);
        let argv0_bytes = read_cstring(&image, argv0);
        assert_eq!(argv0_bytes, b"");

        // argv NULL terminator at offset = 8 (argc) + 8 (argv[0]) = 16.
        assert_eq!(read_u64(&image.bytes, 16), 0);

        // envp NULL terminator at offset 24 (argc + argv[0] + argv NULL).
        assert_eq!(read_u64(&image.bytes, 24), 0);

        // auxv table starts at offset 32. Six 16-byte pairs.
        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
                         // Pair order: PHDR, PHENT, PHNUM, PAGESZ, RANDOM, NULL.
        let pair = |i: usize| {
            let base = auxv_off + i * AUXV_PAIR_SIZE;
            (
                read_u64(&image.bytes, base),
                read_u64(&image.bytes, base + 8),
            )
        };
        assert_eq!(pair(0), (AT_PHDR, 0x4000_0040));
        assert_eq!(pair(1), (AT_PHENT, 56));
        assert_eq!(pair(2), (AT_PHNUM, 7));
        assert_eq!(pair(3), (AT_PAGESZ, 4096));
        assert_eq!(pair(4).0, AT_RANDOM);
        assert_eq!(pair(5), (AT_NULL, 0));

        // AT_RANDOM region is 16 bytes of zero in the image.
        let at_random_ptr = pair(4).1;
        let at_random_off = ptr_to_off(&image, at_random_ptr);
        for i in 0..AT_RANDOM_REGION_SIZE {
            assert_eq!(image.bytes[at_random_off + i], 0);
        }
    }

    #[test]
    fn build_initial_user_stack_round_trip_argv() {
        let stack_top = 0x4000_0000u64;
        let argv: [&[u8]; 2] = [b"hello", b"world"];
        let envp: [&[u8]; 1] = [b"PATH=/bin"];
        let image = build_initial_user_stack(stack_top, &argv, &envp, &facts());

        // argc == 2.
        let argc = read_u64(&image.bytes, 0);
        assert_eq!(argc, 2);

        // Walk argv.
        let argv0 = read_u64(&image.bytes, 8);
        let argv1 = read_u64(&image.bytes, 16);
        let argv_term = read_u64(&image.bytes, 24);
        assert_eq!(read_cstring(&image, argv0), b"hello");
        assert_eq!(read_cstring(&image, argv1), b"world");
        assert_eq!(argv_term, 0);

        // envp[0] sits right after the argv terminator.
        let envp0 = read_u64(&image.bytes, 32);
        let envp_term = read_u64(&image.bytes, 40);
        assert_eq!(read_cstring(&image, envp0), b"PATH=/bin");
        assert_eq!(envp_term, 0);

        // String addresses must be strictly above the upper-table
        // region (they live in the string pool).
        let upper_end = image.initial_sp
            + (WORD_SIZE                                // argc
                + 3 * WORD_SIZE                         // argv ptrs + NULL
                + 2 * WORD_SIZE                         // envp ptrs + NULL
                + AUXV_PAIR_COUNT * AUXV_PAIR_SIZE)     // auxv table
                as u64;
        assert!(argv0 >= upper_end);
        assert!(argv1 >= upper_end);
        assert!(envp0 >= upper_end);
        // Strings should land below stack_top.
        assert!(argv0 < stack_top);
        assert!(envp0 < stack_top);

        // Pointers should be ordered: argv strings below envp strings
        // (string pool layout: argv pool, then envp pool, then top).
        assert!(argv0 < envp0);
        assert!(argv1 < envp0);
    }

    #[test]
    fn build_initial_user_stack_alignment_holds_under_odd_argv_lengths() {
        // 3-character strings (4 bytes including NUL) — would land
        // sp on a non-16-aligned boundary without the alignment
        // padding the builder inserts.
        let stack_top = 0x4000_0000u64;
        let argv: [&[u8]; 3] = [b"abc", b"def", b"ghi"];
        let envp: [&[u8]; 2] = [b"X=1", b"YY=2"];
        let image = build_initial_user_stack(stack_top, &argv, &envp, &facts());
        assert_eq!(
            image.initial_sp & (STACK_ALIGN as u64 - 1),
            0,
            "sp {:#x} not 16-byte aligned",
            image.initial_sp
        );
        // The total image must end exactly at stack_top.
        assert_eq!(image.initial_sp + image.bytes.len() as u64, stack_top);
    }

    #[test]
    fn build_initial_user_stack_at_random_pointer_resolves_into_zero_region() {
        let stack_top = 0x4000_0000u64;
        let argv: [&[u8]; 1] = [b"prog"];
        let envp: [&[u8]; 0] = [];
        let image = build_initial_user_stack(stack_top, &argv, &envp, &facts());

        // Walk to the auxv table.
        let auxv_off = WORD_SIZE              // argc
            + 2 * WORD_SIZE                   // argv[0] + NULL
            + WORD_SIZE; // envp NULL

        // AT_RANDOM is the 5th pair (index 4) in fixed order.
        let at_random_ptr = read_u64(&image.bytes, auxv_off + 4 * AUXV_PAIR_SIZE + 8);
        let at_random_type = read_u64(&image.bytes, auxv_off + 4 * AUXV_PAIR_SIZE);
        assert_eq!(at_random_type, AT_RANDOM);

        let off = ptr_to_off(&image, at_random_ptr);
        for i in 0..AT_RANDOM_REGION_SIZE {
            assert_eq!(image.bytes[off + i], 0, "AT_RANDOM byte {} non-zero", i);
        }
    }

    #[test]
    fn build_initial_user_stack_dummy_argv0_synthesised_when_empty() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts());

        let argc = read_u64(&image.bytes, 0);
        assert_eq!(argc, 1, "empty argv must synthesise argc=1");

        let argv0 = read_u64(&image.bytes, WORD_SIZE);
        let argv_term = read_u64(&image.bytes, 2 * WORD_SIZE);
        assert_ne!(argv0, 0, "argv[0] must be a valid pointer (CVE-2021-4034)");
        assert_eq!(argv_term, 0, "argv NULL terminator at argv[1]");

        // The pointed-at string is a single NUL byte.
        let off = ptr_to_off(&image, argv0);
        assert_eq!(
            image.bytes[off], 0,
            "synthetic argv[0] must be empty C string"
        );
    }
}
