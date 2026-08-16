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
//! and `src/env/__init_tls.c`). The DAC + setuid slice's Part 6 grew
//! the emitted auxv table from six entries to eleven so musl's
//! `__init_security` runtime can read the cred + setuid-binary signal
//! straight off the stack. The drift-cleanup chore added `AT_BASE` and
//! `AT_ENTRY` to bring the minimum table to thirteen entries: `AT_PHDR`,
//! `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_ENTRY`,
//! `AT_UID`, `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_SECURE`, `AT_RANDOM`,
//! `AT_NULL`. Later arch/runtime entries are emitted when present; absent
//! optional pointer entries are omitted, not encoded with null values.

use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Auxv `a_type` constants. Linux RV64 generic ABI; the values are stable
// across architectures (defined in <elf.h> / <linux/auxvec.h>).
//
// These tx-owned local keys define the Linux uABI serialization boundary:
// `AuxvFacts` supplies values, and the stack builder emits key/value pairs.
// The numeric values are frozen by the Linux uABI and remain independent of
// the ELF syntax parser backend.
// ---------------------------------------------------------------------------

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_HWCAP: u64 = 16;
const AT_HWCAP2: u64 = 26;
const AT_PLATFORM: u64 = 15;
const AT_CLKTCK: u64 = 17;
const AT_EXECFN: u64 = 31;
const AT_FLAGS: u64 = 8;
const AT_SYSINFO_EHDR: u64 = 33;
pub const CLKTCK_VALUE: u64 = 100;

/// Auxv pair size in bytes (`a_type: u64, a_val: u64`).
const AUXV_PAIR_SIZE: usize = 16;

/// Width of a single pointer / counter on RV64.
const WORD_SIZE: usize = 8;

/// Keep one readable, zeroed machine word above the final initial-stack C
/// string. Libc startup code may inspect strings a word at a time, and Linux
/// likewise leaves one system word below the stack VMA's exclusive end.
const STACK_TOP_SLACK_SIZE: usize = WORD_SIZE;

/// AT_RANDOM region size (musl reads exactly 16 bytes via
/// `__init_ssp((void*)aux[AT_RANDOM])`).
const AT_RANDOM_REGION_SIZE: usize = 16;

/// SysV RV64 psABI mandates 16-byte stack alignment at `_start`.
const STACK_ALIGN: usize = 16;

/// Maximum number of auxv pairs reserved in the initial stack image.
///
/// Part 6 of the DAC + setuid slice grew this from 6 to 11 by adding
/// `AT_UID`, `AT_EUID`, `AT_GID`, `AT_EGID`, and `AT_SECURE`. The
/// drift-cleanup chore (2026-05-07) grew it from 11 to 13 by adding
/// `AT_BASE` and `AT_ENTRY`; both pairs target musl's
/// `__libc_start_main`, which reads `AT_ENTRY` as the main executable's
/// entry and `AT_BASE` as the interpreter load bias (zero when no
/// interpreter is present). Each new pair is
/// 16 bytes, so the upper-table region grew by 7 × 16 = 112 bytes
/// over the pre-Part-6 baseline; the existing alignment helper handles
/// the size change automatically.
///
/// Optional pointer-valued entries (`AT_PLATFORM`, `AT_SYSINFO_EHDR`,
/// `AT_EXECFN`) are omitted when absent, not emitted with a zero value:
/// musl records presence in `aux[0]` while decoding auxv, and presence
/// with a null pointer is observably different from absence.
const AUXV_PAIR_COUNT: usize = 20;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackBuildError {
    OutOfMemory,
    InvalidString,
    InvalidLayout,
}

fn try_zeroed_stack_bytes(len: usize) -> Result<Vec<u8>, StackBuildError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| StackBuildError::OutOfMemory)?;
    bytes.resize(len, 0);
    Ok(bytes)
}

/// Auxiliary-vector facts the loader knows after parsing the ELF.
///
/// (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`) come from the ELF header and
/// the `PT_PHDR` phdr (or the loader's fallback computation per
/// `txdoc:EXEC-8-6-AT-PHDR-COMPUTATION`); `AT_PAGESZ` is the
/// platform's user page size (4096 on RV64 today). The cred-derived
/// fields (`at_uid`, `at_euid`, `at_gid`, `at_egid`, `at_secure`) reflect
/// the post-`step_apply_suid_for_exec` credentials and whether setuid/setgid
/// changed an effective id.
pub struct AuxvFacts<'a> {
    /// Virtual address of the program-header table after load bias. `ET_EXEC`
    /// images retain their fixed addresses and therefore have zero bias.
    pub at_phdr: u64,
    /// Size of one program header. Always 56 on RV64 ELF64.
    pub at_phent: u64,
    /// Count of program headers.
    pub at_phnum: u64,
    /// Page size. 4096 on RV64.
    pub at_pagesz: u64,
    /// Dynamic interpreter load bias, or `0` when no interpreter is
    /// present. musl's `__libc_start_main` reads this to distinguish
    /// interpreter-loaded binaries from static execution.
    pub at_base: u64,
    /// Main executable entry after load-bias rebasing. For dynamic exec,
    /// `AT_ENTRY` remains the main entry while the initial user PC is the
    /// interpreter entry; those two addresses are intentionally distinct.
    pub at_entry: u64,
    /// Real user id at exec time (caller's `cred.uid`). Read by musl's
    /// `__init_security` to populate `__libc.secure` alongside
    /// `at_secure`.
    pub at_uid: u64,
    /// Effective user id at exec time (`cred.euid`). Differs from
    /// `at_uid` when the binary's `S_ISUID` bit took effect; Wave 4
    /// computes the post-recompute value before this struct is built.
    pub at_euid: u64,
    /// Real group id at exec time (`cred.gid`).
    pub at_gid: u64,
    /// Effective group id at exec time (`cred.egid`). Differs from
    /// `at_gid` when the binary's `S_ISGID` bit took effect.
    pub at_egid: u64,
    /// `1` when the binary executed at exec time was setuid or setgid
    /// (i.e. the effective uid or gid changed at exec); `0` otherwise.
    /// Used by libssp / musl to harden the runtime: clear
    /// `LD_PRELOAD`-equivalent env, force allocator hardening, etc.
    /// `step_apply_suid_for_exec` sets it to `1` when credential recomputation
    /// changes an effective id.
    pub at_secure: u64,
    /// 16 random bytes for the AT_RANDOM auxv slot. musl's
    /// `__init_ssp` reads exactly 16 bytes through this pointer to
    /// seed the stack canary; glibc additionally uses them as
    /// per-process key material. The exec front-end fills these
    /// from the platform's `EntropyIf::fill_random` impl; tests
    /// pass deterministic values (`[0; 16]` or test-chosen bytes)
    /// to keep image-layout assertions stable.
    pub at_random_bytes: [u8; 16],
    pub at_hwcap: u64,
    pub at_hwcap2: u64,
    pub platform_string: &'a [u8],
    pub at_clktck: u64,
    pub execfn_string: &'a [u8],
    pub at_flags: u64,
    pub at_sysinfo_ehdr: Option<u64>,
    pub user_top: u64,
}

/// Build the initial userspace stack image for execve.
///
/// `stack_top` is the first user VA excluded from the stack image;
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
///   │   zeroed top-word slack          │
///   │   string pool (envp strings)     │
///   │   string pool (argv strings)     │
///   │   AT_EXECFN C string              │
///   │   AT_PLATFORM C string            │
///   │   16-byte AT_RANDOM region       │
///   │   (16-byte alignment padding)    │
///   ├──────────────────────────────────┤
///   │   auxv terminator: AT_NULL = 0   │
///   │   auxv: AT_RANDOM ptr -> region  │
///   │   auxv: AT_SECURE                │
///   │   auxv: AT_EGID                  │
///   │   auxv: AT_GID                   │
///   │   auxv: AT_EUID                  │
///   │   auxv: AT_UID                   │
///   │   auxv: AT_ENTRY                 │
///   │   auxv: AT_BASE                  │
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
) -> Result<UserStackImage, StackBuildError> {
    if stack_top == 0 || stack_top > auxv_facts.user_top {
        return Err(StackBuildError::InvalidLayout);
    }
    if auxv_facts.platform_string.contains(&0)
        || auxv_facts.execfn_string.contains(&0)
        || argv.iter().any(|string| string.contains(&0))
        || envp.iter().any(|string| string.contains(&0))
    {
        return Err(StackBuildError::InvalidString);
    }

    let checked_cstring_len = |bytes: &[u8]| {
        bytes
            .len()
            .checked_add(1)
            .ok_or(StackBuildError::InvalidLayout)
    };
    let checked_strings_len = |strings: &[&[u8]]| {
        strings.iter().try_fold(0usize, |total, string| {
            total
                .checked_add(checked_cstring_len(string)?)
                .ok_or(StackBuildError::InvalidLayout)
        })
    };

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
    let argv_string_total = checked_strings_len(argv_view)?;
    let envp_string_total = checked_strings_len(envp)?;
    let platform_string_total = if auxv_facts.platform_string.is_empty() {
        0
    } else {
        checked_cstring_len(auxv_facts.platform_string)?
    };
    let execfn_string_total = if auxv_facts.execfn_string.is_empty() {
        0
    } else {
        checked_cstring_len(auxv_facts.execfn_string)?
    };
    let string_pool_size = argv_string_total
        .checked_add(envp_string_total)
        .and_then(|total| total.checked_add(platform_string_total))
        .and_then(|total| total.checked_add(execfn_string_total))
        .ok_or(StackBuildError::InvalidLayout)?;

    // ---- 3. Compute the upper-table size --------------------------------
    //
    //   argc (8) + (argc+1) argv ptrs + (envc+1) envp ptrs + reserved auxv
    //
    // The auxv reservation is fixed at `AUXV_PAIR_COUNT`; the actual
    // emitted list can be shorter when optional pointer facts are absent.
    let argv_pointer_bytes = argc
        .checked_add(1)
        .and_then(|count| count.checked_mul(WORD_SIZE))
        .ok_or(StackBuildError::InvalidLayout)?;
    let envp_pointer_bytes = envc
        .checked_add(1)
        .and_then(|count| count.checked_mul(WORD_SIZE))
        .ok_or(StackBuildError::InvalidLayout)?;
    let auxv_bytes = AUXV_PAIR_COUNT
        .checked_mul(AUXV_PAIR_SIZE)
        .ok_or(StackBuildError::InvalidLayout)?;
    let upper_table_size = WORD_SIZE
        .checked_add(argv_pointer_bytes)
        .and_then(|size| size.checked_add(envp_pointer_bytes))
        .and_then(|size| size.checked_add(auxv_bytes))
        .ok_or(StackBuildError::InvalidLayout)?;

    // ---- 4. Compute total bytes and 16-byte alignment padding ------------
    //
    // Layout (high → low):
    //   top_slack      (one zeroed machine word)
    //   string_pool    (envp, argv, execfn, platform strings)
    //   AT_RANDOM      (16 bytes, also 16-byte aligned)
    //   pad            (so initial_sp = stack_top - total ends up 16-aligned)
    //   upper_table    (argc + ptrs + auxv)
    //
    // We want `initial_sp = stack_top - total` to be 16-byte aligned.
    // `stack_top` itself is required by callers to be page-aligned
    // (and therefore 16-byte aligned) — but we do not lean on that
    // assumption: we compute padding from `stack_top` modulo 16
    // explicitly.
    let unpadded = upper_table_size
        .checked_add(AT_RANDOM_REGION_SIZE)
        .and_then(|size| size.checked_add(string_pool_size))
        .and_then(|size| size.checked_add(STACK_TOP_SLACK_SIZE))
        .ok_or(StackBuildError::InvalidLayout)?;
    let stack_top_mod = usize::try_from(stack_top & (STACK_ALIGN as u64 - 1))
        .map_err(|_| StackBuildError::InvalidLayout)?;
    // We need (stack_top - total) mod 16 == 0
    //   <=> total mod 16 == stack_top mod 16
    let need = stack_top_mod;
    let have = unpadded & (STACK_ALIGN - 1);
    let pad = (need + STACK_ALIGN - have) & (STACK_ALIGN - 1);
    let total = unpadded
        .checked_add(pad)
        .ok_or(StackBuildError::InvalidLayout)?;
    let total_u64 = u64::try_from(total).map_err(|_| StackBuildError::InvalidLayout)?;
    let initial_sp = stack_top
        .checked_sub(total_u64)
        .ok_or(StackBuildError::InvalidLayout)?;
    if !initial_sp.is_multiple_of(STACK_ALIGN as u64)
        || initial_sp >= auxv_facts.user_top
        || initial_sp.checked_add(total_u64) != Some(stack_top)
    {
        return Err(StackBuildError::InvalidLayout);
    }

    // ---- 5. Compute key user-VA addresses --------------------------------
    //
    // String-pool layout (low → high addresses):
    //   platform, execfn, argv, envp, zeroed top-word slack.
    //
    // The string pool sits above the AT_RANDOM region and below the
    // compatibility slack at the stack recipe's exclusive upper bound:
    //
    //     stack_top
    //     [zeroed top-word slack]
    //     [envp strings]
    //     [argv strings]
    //     [AT_EXECFN]
    //     [AT_PLATFORM]
    //     [AT_RANDOM 16B]
    //     [pad]
    //     [upper_table]
    //
    // But musl reads AT_RANDOM via a pointer; its physical position
    // in the pool is irrelevant as long as the auxv entry's a_val
    // points at 16 contiguous bytes. We pin the layout above for
    // determinism. The string pool occupies the bytes immediately below the
    // top-word slack; the AT_RANDOM region sits below all four string groups.
    //
    // Concretely:
    //   envp_pool_top  = stack_top - STACK_TOP_SLACK_SIZE
    //   envp_pool_base = envp_pool_top - envp_string_total
    //   argv_pool_top  = envp_pool_base
    //   argv_pool_base = argv_pool_top - argv_string_total
    //   execfn_pool_top = argv_pool_base
    //   execfn_pool_base = execfn_pool_top - execfn_string_total
    //   platform_pool_top = execfn_pool_base
    //   platform_pool_base = platform_pool_top - platform_string_total
    //   at_random_top  = platform_pool_base
    //   at_random_base = at_random_top - AT_RANDOM_REGION_SIZE
    //
    // upper_table_top    = at_random_base - pad
    // upper_table_base   = upper_table_top - upper_table_size
    //                    = initial_sp
    let envp_pool_top = stack_top
        .checked_sub(
            u64::try_from(STACK_TOP_SLACK_SIZE).map_err(|_| StackBuildError::InvalidLayout)?,
        )
        .ok_or(StackBuildError::InvalidLayout)?;
    let envp_pool_base = envp_pool_top
        .checked_sub(u64::try_from(envp_string_total).map_err(|_| StackBuildError::InvalidLayout)?)
        .ok_or(StackBuildError::InvalidLayout)?;
    let argv_pool_top = envp_pool_base;
    let argv_pool_base = argv_pool_top
        .checked_sub(u64::try_from(argv_string_total).map_err(|_| StackBuildError::InvalidLayout)?)
        .ok_or(StackBuildError::InvalidLayout)?;
    let execfn_pool_top = argv_pool_base;
    let execfn_pool_base = execfn_pool_top
        .checked_sub(
            u64::try_from(execfn_string_total).map_err(|_| StackBuildError::InvalidLayout)?,
        )
        .ok_or(StackBuildError::InvalidLayout)?;
    let platform_pool_top = execfn_pool_base;
    let platform_pool_base = platform_pool_top
        .checked_sub(
            u64::try_from(platform_string_total).map_err(|_| StackBuildError::InvalidLayout)?,
        )
        .ok_or(StackBuildError::InvalidLayout)?;
    let at_random_top = platform_pool_base;
    let at_random_base = at_random_top
        .checked_sub(AT_RANDOM_REGION_SIZE as u64)
        .ok_or(StackBuildError::InvalidLayout)?;
    let at_platform = (!auxv_facts.platform_string.is_empty()).then_some(platform_pool_base);
    let at_execfn = (!auxv_facts.execfn_string.is_empty()).then_some(execfn_pool_base);
    if at_random_base < initial_sp
        || at_random_base >= auxv_facts.user_top
        || [at_platform, at_execfn]
            .into_iter()
            .flatten()
            .any(|address| address < initial_sp || address >= auxv_facts.user_top)
    {
        return Err(StackBuildError::InvalidLayout);
    }

    // ---- 6. Allocate the kernel buffer ----------------------------------
    //
    // `bytes[0]` corresponds to `initial_sp` (the lowest byte we
    // emit). `bytes[total - 1]` corresponds to `stack_top - 1`.
    let mut bytes = try_zeroed_stack_bytes(total)?;

    // Helper: write a u64 (little-endian) at offset `off` in `bytes`.
    // RV64 is little-endian; psABI fixes `Elf64_addr`/`Elf64_xword`
    // as LE on RISC-V.
    fn write_u64(bytes: &mut [u8], off: usize, v: u64) -> Result<(), StackBuildError> {
        let end = off
            .checked_add(WORD_SIZE)
            .ok_or(StackBuildError::InvalidLayout)?;
        bytes
            .get_mut(off..end)
            .ok_or(StackBuildError::InvalidLayout)?
            .copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    // ---- 7. Write the upper table (argc, argv ptrs, envp ptrs, auxv) ----
    //
    // Offsets are relative to `initial_sp` == bytes[0].
    let mut off: usize = 0;

    // argc
    write_u64(
        &mut bytes,
        off,
        u64::try_from(argc).map_err(|_| StackBuildError::InvalidLayout)?,
    )?;
    off = off
        .checked_add(WORD_SIZE)
        .ok_or(StackBuildError::InvalidLayout)?;

    // argv pointers — point into the string pool; layout the strings
    // densely starting at `argv_pool_base`.
    let mut argv_string_cursor = argv_pool_base;
    for s in argv_view {
        write_u64(&mut bytes, off, argv_string_cursor)?;
        off = off
            .checked_add(WORD_SIZE)
            .ok_or(StackBuildError::InvalidLayout)?;
        argv_string_cursor = argv_string_cursor
            .checked_add(
                u64::try_from(checked_cstring_len(s)?)
                    .map_err(|_| StackBuildError::InvalidLayout)?,
            )
            .ok_or(StackBuildError::InvalidLayout)?;
    }
    // argv NULL terminator
    write_u64(&mut bytes, off, 0)?;
    off = off
        .checked_add(WORD_SIZE)
        .ok_or(StackBuildError::InvalidLayout)?;
    if argv_string_cursor != argv_pool_top {
        return Err(StackBuildError::InvalidLayout);
    }

    // envp pointers
    let mut envp_string_cursor = envp_pool_base;
    for s in envp {
        write_u64(&mut bytes, off, envp_string_cursor)?;
        off = off
            .checked_add(WORD_SIZE)
            .ok_or(StackBuildError::InvalidLayout)?;
        envp_string_cursor = envp_string_cursor
            .checked_add(
                u64::try_from(checked_cstring_len(s)?)
                    .map_err(|_| StackBuildError::InvalidLayout)?,
            )
            .ok_or(StackBuildError::InvalidLayout)?;
    }
    // envp NULL terminator
    write_u64(&mut bytes, off, 0)?;
    off = off
        .checked_add(WORD_SIZE)
        .ok_or(StackBuildError::InvalidLayout)?;
    if envp_string_cursor != envp_pool_top {
        return Err(StackBuildError::InvalidLayout);
    }

    // auxv (fixed order matching Linux `fs/binfmt_elf.c::create_elf_tables`
    // for the slice's surface). Optional pointer-valued entries are
    // included only when they point at a real stack/user object.
    let mut auxv_entries: [(u64, u64); AUXV_PAIR_COUNT] = [(AT_NULL, 0); AUXV_PAIR_COUNT];
    let mut auxv_len = 0usize;
    {
        let mut push_auxv = |a_type: u64, a_val: u64| -> Result<(), StackBuildError> {
            if auxv_len >= AUXV_PAIR_COUNT {
                return Err(StackBuildError::InvalidLayout);
            }
            auxv_entries[auxv_len] = (a_type, a_val);
            auxv_len += 1;
            Ok(())
        };
        push_auxv(AT_PHDR, auxv_facts.at_phdr)?;
        push_auxv(AT_PHENT, auxv_facts.at_phent)?;
        push_auxv(AT_PHNUM, auxv_facts.at_phnum)?;
        push_auxv(AT_PAGESZ, auxv_facts.at_pagesz)?;
        push_auxv(AT_BASE, auxv_facts.at_base)?;
        push_auxv(AT_ENTRY, auxv_facts.at_entry)?;
        push_auxv(AT_UID, auxv_facts.at_uid)?;
        push_auxv(AT_EUID, auxv_facts.at_euid)?;
        push_auxv(AT_GID, auxv_facts.at_gid)?;
        push_auxv(AT_EGID, auxv_facts.at_egid)?;
        push_auxv(AT_SECURE, auxv_facts.at_secure)?;
        push_auxv(AT_RANDOM, at_random_base)?;
        push_auxv(AT_HWCAP, auxv_facts.at_hwcap)?;
        push_auxv(AT_HWCAP2, auxv_facts.at_hwcap2)?;
        if let Some(at_platform) = at_platform {
            push_auxv(AT_PLATFORM, at_platform)?;
        }
        push_auxv(AT_CLKTCK, auxv_facts.at_clktck)?;
        if let Some(at_sysinfo_ehdr) = auxv_facts.at_sysinfo_ehdr.filter(|v| *v != 0) {
            push_auxv(AT_SYSINFO_EHDR, at_sysinfo_ehdr)?;
        }
        if let Some(at_execfn) = at_execfn {
            push_auxv(AT_EXECFN, at_execfn)?;
        }
        push_auxv(AT_FLAGS, auxv_facts.at_flags)?;
        push_auxv(AT_NULL, 0)?;
    }
    for &(a_type, a_val) in auxv_entries[..auxv_len].iter() {
        write_u64(&mut bytes, off, a_type)?;
        let value_off = off
            .checked_add(WORD_SIZE)
            .ok_or(StackBuildError::InvalidLayout)?;
        write_u64(&mut bytes, value_off, a_val)?;
        off = off
            .checked_add(AUXV_PAIR_SIZE)
            .ok_or(StackBuildError::InvalidLayout)?;
    }
    if off > upper_table_size {
        return Err(StackBuildError::InvalidLayout);
    }

    // ---- 8. Padding region is already zeroed -----------------------------

    // ---- 9. AT_RANDOM region: 16 bytes from auxv_facts -----------------
    //
    // The exec front-end fills `at_random_bytes` from the platform's
    // `EntropyIf::fill_random` (RV64: rdtime + xorshift counter;
    // other boards: deterministic counter default). Tests pass
    // explicit bytes (often `[0; 16]`) to keep layout-pin
    // assertions stable.
    let at_random_off = upper_table_size
        .checked_add(pad)
        .ok_or(StackBuildError::InvalidLayout)?;
    let at_random_end = at_random_off
        .checked_add(AT_RANDOM_REGION_SIZE)
        .ok_or(StackBuildError::InvalidLayout)?;
    bytes
        .get_mut(at_random_off..at_random_end)
        .ok_or(StackBuildError::InvalidLayout)?
        .copy_from_slice(&auxv_facts.at_random_bytes);

    fn write_cstring(
        bytes: &mut [u8],
        offset: usize,
        string: &[u8],
    ) -> Result<usize, StackBuildError> {
        let string_end = offset
            .checked_add(string.len())
            .ok_or(StackBuildError::InvalidLayout)?;
        bytes
            .get_mut(offset..string_end)
            .ok_or(StackBuildError::InvalidLayout)?
            .copy_from_slice(string);
        let nul = bytes
            .get_mut(string_end)
            .ok_or(StackBuildError::InvalidLayout)?;
        *nul = 0;
        string_end
            .checked_add(1)
            .ok_or(StackBuildError::InvalidLayout)
    }

    fn user_offset(address: u64, initial_sp: u64) -> Result<usize, StackBuildError> {
        address
            .checked_sub(initial_sp)
            .ok_or(StackBuildError::InvalidLayout)
            .and_then(|offset| usize::try_from(offset).map_err(|_| StackBuildError::InvalidLayout))
    }

    let platform_pool_off = user_offset(platform_pool_base, initial_sp)?;
    let mut cursor = platform_pool_off;
    if !auxv_facts.platform_string.is_empty() {
        cursor = write_cstring(&mut bytes, cursor, auxv_facts.platform_string)?;
    }
    if cursor != user_offset(platform_pool_top, initial_sp)? {
        return Err(StackBuildError::InvalidLayout);
    }

    let execfn_pool_off = user_offset(execfn_pool_base, initial_sp)?;
    let mut cursor = execfn_pool_off;
    if !auxv_facts.execfn_string.is_empty() {
        cursor = write_cstring(&mut bytes, cursor, auxv_facts.execfn_string)?;
    }
    if cursor != user_offset(execfn_pool_top, initial_sp)? {
        return Err(StackBuildError::InvalidLayout);
    }

    // ---- 10. argv string pool ------------------------------------------
    //
    // Lives at user-VA `argv_pool_base`, which corresponds to byte
    // offset `argv_pool_base - initial_sp`.
    let argv_pool_off = user_offset(argv_pool_base, initial_sp)?;
    let mut cursor = argv_pool_off;
    for s in argv_view {
        cursor = write_cstring(&mut bytes, cursor, s)?;
    }
    if cursor != user_offset(argv_pool_top, initial_sp)? {
        return Err(StackBuildError::InvalidLayout);
    }

    // ---- 11. envp string pool ------------------------------------------
    let envp_pool_off = user_offset(envp_pool_base, initial_sp)?;
    let mut cursor = envp_pool_off;
    for s in envp {
        cursor = write_cstring(&mut bytes, cursor, s)?;
    }
    if cursor != user_offset(envp_pool_top, initial_sp)? {
        return Err(StackBuildError::InvalidLayout);
    }

    Ok(UserStackImage { initial_sp, bytes })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_zeroed_buffer_reports_capacity_as_out_of_memory() {
        assert_eq!(
            try_zeroed_stack_bytes(usize::MAX),
            Err(StackBuildError::OutOfMemory)
        );
    }

    #[test]
    fn stack_builder_rejects_user_top_and_aux_string_contract_violations() {
        let stack_top = 0x4000_0000u64;
        assert!(matches!(
            build_initial_user_stack(
                stack_top,
                &[],
                &[],
                &AuxvFacts {
                    user_top: stack_top - 1,
                    ..facts()
                },
            ),
            Err(StackBuildError::InvalidLayout)
        ));
        assert!(matches!(
            build_initial_user_stack(
                stack_top,
                &[],
                &[],
                &AuxvFacts {
                    execfn_string: b"/bin/app\0spoofed",
                    ..facts()
                },
            ),
            Err(StackBuildError::InvalidString)
        ));
        assert!(matches!(
            build_initial_user_stack(stack_top, &[b"arg\0tail"], &[], &facts(),),
            Err(StackBuildError::InvalidString)
        ));
        assert!(matches!(
            build_initial_user_stack(stack_top, &[], &[b"KEY=value\0tail"], &facts(),),
            Err(StackBuildError::InvalidString)
        ));
        assert!(matches!(
            build_initial_user_stack(
                stack_top,
                &[],
                &[],
                &AuxvFacts {
                    platform_string: b"riscv64\0spoofed",
                    ..facts()
                },
            ),
            Err(StackBuildError::InvalidString)
        ));
        assert!(matches!(
            build_initial_user_stack(
                8,
                &[],
                &[],
                &AuxvFacts {
                    user_top: 8,
                    ..facts()
                },
            ),
            Err(StackBuildError::InvalidLayout)
        ));
    }

    /// Compose a default `AuxvFacts` whose values are easy to spot in
    /// hex dumps when debugging a layout regression. The cred-derived
    /// fields default to the bootstrap-init shape (`uid = euid = 0`)
    /// with `at_secure = 0`; tests that need a setuid-binary
    /// signature override `at_secure` (and the eid fields) inline.
    fn facts() -> AuxvFacts<'static> {
        AuxvFacts {
            at_phdr: 0x4000_0040,
            at_phent: 56,
            at_phnum: 7,
            at_pagesz: 4096,
            at_base: 0,
            at_entry: 0,
            at_uid: 0,
            at_euid: 0,
            at_gid: 0,
            at_egid: 0,
            at_secure: 0,
            at_random_bytes: [0u8; 16],
            at_hwcap: 0,
            at_hwcap2: 0,
            platform_string: b"",
            at_clktck: CLKTCK_VALUE,
            execfn_string: b"",
            at_flags: 0,
            at_sysinfo_ehdr: None,
            user_top: 0x4000_0000,
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
        let image = build_initial_user_stack(stack_top, &[], &[], &facts()).expect("stack image");

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

        // auxv table starts at offset 32. Optional pointer-valued
        // facts are omitted when absent, so this minimum image reaches
        // AT_NULL earlier than the reserved table capacity.
        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
                         // Pair order: PHDR, PHENT, PHNUM, PAGESZ,
                         // BASE, ENTRY, UID, EUID, GID, EGID, SECURE,
                         // RANDOM, HWCAP, HWCAP2, CLKTCK, FLAGS, NULL.
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
        assert_eq!(pair(4), (AT_BASE, 0));
        assert_eq!(pair(5), (AT_ENTRY, 0));
        assert_eq!(pair(6), (AT_UID, 0));
        assert_eq!(pair(7), (AT_EUID, 0));
        assert_eq!(pair(8), (AT_GID, 0));
        assert_eq!(pair(9), (AT_EGID, 0));
        assert_eq!(pair(10), (AT_SECURE, 0));
        assert_eq!(pair(11).0, AT_RANDOM);
        assert_eq!(pair(12).0, AT_HWCAP);
        assert_eq!(pair(13).0, AT_HWCAP2);
        assert_eq!(pair(14).0, AT_CLKTCK);
        assert_eq!(pair(15).0, AT_FLAGS);
        assert_eq!(pair(16), (AT_NULL, 0));
        assert_eq!(pair(17), (AT_NULL, 0));
        assert_eq!(pair(18), (AT_NULL, 0));
        assert_eq!(pair(19), (AT_NULL, 0));

        // AT_RANDOM region is 16 bytes of zero in the image.
        let at_random_ptr = pair(11).1;
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
        let image =
            build_initial_user_stack(stack_top, &argv, &envp, &facts()).expect("stack image");

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
    fn build_initial_user_stack_keeps_a_zero_word_above_the_last_cstring() {
        let stack_top = 0x4000_0000u64;
        let argv: [&[u8]; 1] = [b"/init"];
        let envp: [&[u8]; 1] = [b"PATH=/bin"];
        let image =
            build_initial_user_stack(stack_top, &argv, &envp, &facts()).expect("stack image");

        let envp0 = read_u64(&image.bytes, 3 * WORD_SIZE);
        assert_eq!(read_cstring(&image, envp0), envp[0]);

        let first_byte_after_envp = envp0 + envp[0].len() as u64 + 1;
        assert_eq!(
            stack_top - first_byte_after_envp,
            WORD_SIZE as u64,
            "libc word-at-a-time probes need one readable word above the final C string"
        );
        assert_eq!(
            &image.bytes[image.bytes.len() - WORD_SIZE..],
            &[0; WORD_SIZE],
            "top-stack compatibility slack must be explicitly zeroed"
        );
    }

    #[test]
    fn build_initial_user_stack_alignment_holds_under_odd_argv_lengths() {
        // 3-character strings (4 bytes including NUL) — would land
        // sp on a non-16-aligned boundary without the alignment
        // padding the builder inserts.
        let stack_top = 0x4000_0000u64;
        let argv: [&[u8]; 3] = [b"abc", b"def", b"ghi"];
        let envp: [&[u8]; 2] = [b"X=1", b"YY=2"];
        let image =
            build_initial_user_stack(stack_top, &argv, &envp, &facts()).expect("stack image");
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
        let image =
            build_initial_user_stack(stack_top, &argv, &envp, &facts()).expect("stack image");

        // Walk to the auxv table.
        let auxv_off = WORD_SIZE              // argc
            + 2 * WORD_SIZE                   // argv[0] + NULL
            + WORD_SIZE; // envp NULL

        // AT_RANDOM is the 12th pair (index 11) in fixed order
        // post-drift-cleanup: PHDR, PHENT, PHNUM, PAGESZ, BASE, ENTRY,
        // UID, EUID, GID, EGID, SECURE, RANDOM, NULL.
        let at_random_ptr = read_u64(&image.bytes, auxv_off + 11 * AUXV_PAIR_SIZE + 8);
        let at_random_type = read_u64(&image.bytes, auxv_off + 11 * AUXV_PAIR_SIZE);
        assert_eq!(at_random_type, AT_RANDOM);

        let off = ptr_to_off(&image, at_random_ptr);
        for i in 0..AT_RANDOM_REGION_SIZE {
            assert_eq!(image.bytes[off + i], 0, "AT_RANDOM byte {} non-zero", i);
        }
    }

    #[test]
    fn build_initial_user_stack_dummy_argv0_synthesised_when_empty() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts()).expect("stack image");

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

    /// Walk a freshly built stack and round-trip-decode every auxv
    /// entry, verifying the minimum auxv shape introduced in the
    /// drift-cleanup chore (2026-05-07). Pre-Part-6 the table was six
    /// pairs (PHDR, PHENT, PHNUM, PAGESZ, RANDOM, NULL); Part 6
    /// inserted UID, EUID, GID, EGID, SECURE between PAGESZ and
    /// RANDOM (eleven pairs); the drift-cleanup chore inserted BASE
    /// and ENTRY between PAGESZ and UID. Optional pointer-valued
    /// facts must be omitted when absent because musl tracks auxv
    /// presence separately from auxv value.
    #[test]
    fn build_initial_user_stack_omits_absent_optional_auxv_entries() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts()).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
        let pair = |i: usize| {
            let base = auxv_off + i * AUXV_PAIR_SIZE;
            (
                read_u64(&image.bytes, base),
                read_u64(&image.bytes, base + 8),
            )
        };

        // Reserved capacity stays high enough for optional arch/runtime
        // entries, but absent optional pointer facts do not occupy slots.
        assert_eq!(AUXV_PAIR_COUNT, 20);
        assert_eq!(pair(0).0, AT_PHDR);
        assert_eq!(pair(1).0, AT_PHENT);
        assert_eq!(pair(2).0, AT_PHNUM);
        assert_eq!(pair(3).0, AT_PAGESZ);
        assert_eq!(pair(4).0, AT_BASE);
        assert_eq!(pair(5).0, AT_ENTRY);
        assert_eq!(pair(6).0, AT_UID);
        assert_eq!(pair(7).0, AT_EUID);
        assert_eq!(pair(8).0, AT_GID);
        assert_eq!(pair(9).0, AT_EGID);
        assert_eq!(pair(10).0, AT_SECURE);
        assert_eq!(pair(11).0, AT_RANDOM);
        assert_eq!(pair(12).0, AT_HWCAP);
        assert_eq!(pair(13).0, AT_HWCAP2);
        assert_eq!(pair(14).0, AT_CLKTCK);
        assert_eq!(pair(15).0, AT_FLAGS);
        assert_eq!(pair(16), (AT_NULL, 0));
        assert_eq!(pair(17), (AT_NULL, 0));
        assert_eq!(pair(18), (AT_NULL, 0));
        assert_eq!(pair(19), (AT_NULL, 0));
    }

    #[test]
    fn build_initial_user_stack_emits_optional_auxv_entries_when_present() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            platform_string: b"riscv64",
            at_sysinfo_ehdr: Some(0x4000_2000),
            execfn_string: b"/init",
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
        let pair = |i: usize| {
            let base = auxv_off + i * AUXV_PAIR_SIZE;
            (
                read_u64(&image.bytes, base),
                read_u64(&image.bytes, base + 8),
            )
        };

        assert_eq!(pair(14).0, AT_PLATFORM);
        assert_eq!(read_cstring(&image, pair(14).1), b"riscv64");
        assert_eq!(pair(15), (AT_CLKTCK, CLKTCK_VALUE));
        assert_eq!(pair(16), (AT_SYSINFO_EHDR, 0x4000_2000));
        assert_eq!(pair(17).0, AT_EXECFN);
        assert_eq!(read_cstring(&image, pair(17).1), b"/init");
        assert_eq!(pair(18), (AT_FLAGS, 0));
        assert_eq!(pair(19), (AT_NULL, 0));
    }

    #[test]
    fn build_initial_user_stack_places_platform_and_execfn_strings() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            platform_string: b"riscv64",
            execfn_string: b"/original/script",
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE + 2 * WORD_SIZE + WORD_SIZE;
        let mut platform = None;
        let mut execfn = None;
        for index in 0..AUXV_PAIR_COUNT {
            let base = auxv_off + index * AUXV_PAIR_SIZE;
            match read_u64(&image.bytes, base) {
                AT_PLATFORM => platform = Some(read_u64(&image.bytes, base + WORD_SIZE)),
                AT_EXECFN => execfn = Some(read_u64(&image.bytes, base + WORD_SIZE)),
                AT_NULL => break,
                _ => {}
            }
        }

        let platform = platform.expect("AT_PLATFORM");
        let execfn = execfn.expect("AT_EXECFN");
        assert_eq!(read_cstring(&image, platform), b"riscv64");
        assert_eq!(read_cstring(&image, execfn), b"/original/script");
        assert!(platform < stack_top);
        assert!(execfn < stack_top);
        assert_eq!(image.initial_sp & (STACK_ALIGN as u64 - 1), 0);
    }

    /// AT_UID lives at index 6 of the auxv table — the first cred-
    /// derived entry, sitting immediately after AT_ENTRY. Pin the
    /// position so a future refactor that reorders entries breaks
    /// loudly here rather than silently in musl's
    /// `__init_security`. Pre-drift-cleanup it sat at index 4
    /// (immediately after AT_PAGESZ); the chore inserted AT_BASE
    /// and AT_ENTRY ahead of it.
    #[test]
    fn build_initial_user_stack_emits_at_uid_at_index_6() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            at_uid: 1001,
            at_euid: 1000,
            at_gid: 1001,
            at_egid: 1000,
            at_secure: 1,
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
        let pair_at = |i: usize| {
            let base = auxv_off + i * AUXV_PAIR_SIZE;
            (
                read_u64(&image.bytes, base),
                read_u64(&image.bytes, base + 8),
            )
        };
        assert_eq!(pair_at(6), (AT_UID, 1001));
        assert_eq!(pair_at(7), (AT_EUID, 1000));
        assert_eq!(pair_at(8), (AT_GID, 1001));
        assert_eq!(pair_at(9), (AT_EGID, 1000));
    }

    /// `at_secure = 1` round-trips into the auxv slot at index 10
    /// post-drift-cleanup (was index 8 before AT_BASE and AT_ENTRY
    /// were inserted ahead of the cred fields). Wave 4's
    /// `step_apply_suid_for_exec` sets this when the binary's
    /// setuid/setgid bit caused an effective-id change at exec;
    /// libssp/musl reads it to harden the runtime.
    #[test]
    fn build_initial_user_stack_emits_at_secure_when_facts_set_to_1() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            at_secure: 1,
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
        let secure_base = auxv_off + 10 * AUXV_PAIR_SIZE;
        assert_eq!(read_u64(&image.bytes, secure_base), AT_SECURE);
        assert_eq!(read_u64(&image.bytes, secure_base + 8), 1);
    }

    /// `at_secure = 0` (the Wave 3 hardcoded value, and the post-Wave-4
    /// value for non-setuid binaries) round-trips faithfully.
    #[test]
    fn build_initial_user_stack_emits_at_secure_zero_when_facts_set_to_0() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts()).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL
        let secure_base = auxv_off + 10 * AUXV_PAIR_SIZE;
        assert_eq!(read_u64(&image.bytes, secure_base), AT_SECURE);
        assert_eq!(read_u64(&image.bytes, secure_base + 8), 0);
    }

    /// The 16 bytes the auxv table's AT_RANDOM pointer points at
    /// must be exactly the bytes the caller passed in
    /// `AuxvFacts.at_random_bytes`. The CSPRNG slice (chore branch
    /// `chore/csprng-at-random`) replaces the prior constant
    /// `[0; 16]` with bytes the exec front-end pulls from
    /// `EntropyIf::fill_random`; this test pins the plumbing so a
    /// future regression that drops the copy lands here.
    #[test]
    fn build_initial_user_stack_uses_at_random_bytes_from_auxv_facts() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            at_random_bytes: [0xab; 16],
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL

        // AT_RANDOM is the 12th pair (index 11) in the post-drift-
        // cleanup ordering: PHDR, PHENT, PHNUM, PAGESZ, BASE, ENTRY,
        // UID, EUID, GID, EGID, SECURE, RANDOM, NULL.
        let at_random_type = read_u64(&image.bytes, auxv_off + 11 * AUXV_PAIR_SIZE);
        let at_random_ptr = read_u64(&image.bytes, auxv_off + 11 * AUXV_PAIR_SIZE + 8);
        assert_eq!(at_random_type, AT_RANDOM);

        let off = ptr_to_off(&image, at_random_ptr);
        for i in 0..AT_RANDOM_REGION_SIZE {
            assert_eq!(
                image.bytes[off + i],
                0xab,
                "AT_RANDOM byte {} did not round-trip from auxv_facts",
                i
            );
        }
    }

    /// `AT_ENTRY` carries the `image_plan.entry` value from the ELF
    /// loader through to musl's `__libc_start_main`, which uses it
    /// to detect a non-canonical entry point. Pin the round-trip so
    /// an exec_script Phase 5 regression that drops the field lands
    /// loudly here.
    #[test]
    fn build_initial_user_stack_emits_at_entry_carries_image_plan_entry() {
        let stack_top = 0x4000_0000u64;
        let auxv_facts = AuxvFacts {
            at_entry: 0x1234_5678,
            ..facts()
        };
        let image =
            build_initial_user_stack(stack_top, &[], &[], &auxv_facts).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL

        // AT_ENTRY is the 6th pair (index 5) in the post-drift-cleanup
        // ordering: PHDR, PHENT, PHNUM, PAGESZ, BASE, ENTRY, UID, ...
        let entry_base = auxv_off + 5 * AUXV_PAIR_SIZE;
        assert_eq!(read_u64(&image.bytes, entry_base), AT_ENTRY);
        assert_eq!(read_u64(&image.bytes, entry_base + 8), 0x1234_5678);
    }

    /// A static executable has no interpreter, so its `AT_BASE` remains
    /// zero. Dynamic exec tests cover the complementary nonzero interpreter
    /// load bias.
    #[test]
    fn build_initial_user_stack_emits_at_base_zero_for_static_exec() {
        let stack_top = 0x4000_0000u64;
        let image = build_initial_user_stack(stack_top, &[], &[], &facts()).expect("stack image");

        let auxv_off = WORD_SIZE                // argc
            + 2 * WORD_SIZE                     // argv[0] + NULL
            + WORD_SIZE; // envp NULL

        // AT_BASE is the 5th pair (index 4) in the post-drift-cleanup
        // ordering: PHDR, PHENT, PHNUM, PAGESZ, BASE, ENTRY, UID, ...
        let base_base = auxv_off + 4 * AUXV_PAIR_SIZE;
        assert_eq!(read_u64(&image.bytes, base_base), AT_BASE);
        assert_eq!(read_u64(&image.bytes, base_base + 8), 0);
    }
}
