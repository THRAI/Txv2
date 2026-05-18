# txKernel vDSO + musl-static Bootstrap ABI Design

<!-- txdoc:VDSO-TLS-BOOTSTRAP-V1 -->

**Status:** Draft v0.1
**Target:** RISC-V 64, static musl + BusyBox bringup
**Supersedes:** earlier plan where kernel parsed PT_TLS and initialized initial TLS before vDSO validation. The original phase split covered VVAR, vDSO ELF, loader, per-process mapping, PT_TLS parsing, TLS initialization, and end-to-end validation; this version revises the TLS phases after Phase 0 ABI review.

**Companion documents:**

- [`VM_v1_2.md`](VM_v1_2.md) — AddressSpace, VmEntry, recipes, page-backed backing.
- [`PAGE_BACKED_v1.md`](PAGE_BACKED_v1.md) — PageContainer, RNodeBacking, file/anonymous/device unification.
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — execve spec, PoNR discipline.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP-*, SCRIPT-*, SIG-* rules.
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — derived materializations, publication rule.

---

## 1. Goal

<!-- txdoc:VDSO-GOAL-1 -->

This design has two separate goals:

1. **Make static musl reliably bootstrap.**

   * Correct initial stack.
   * Correct auxv.
   * Working anonymous `mmap`.
   * Working `set_tid_address`.
   * Correct preservation of user `tp` register.

2. **Add vDSO as a later fast-path ABI.**

   * First as a valid ELF image with stub symbols.
   * Later with real `clock_gettime` support through VVAR.

The key decision is:

> For static musl v1, txKernel does **not** construct the initial TLS block in `execve`. musl initializes TLS in userspace from auxv and program headers. Kernel-side TLS parsing is deferred.

---

## 2. Non-goals

This design does **not** implement:

```text
- dynamic linker support
- kernel-built initial TLS for arbitrary libc
- clone(CLONE_SETTLS)
- rseq
- per-CPU VVAR
- high-resolution vDSO clock before timekeeper/counter semantics are stable
- vDSO syscall tracing
```

It also does not make vDSO part of the semantic subsystem. vDSO is an ABI materialization mapped into user address spaces, not a syscall script and not a step participant.

---

## 3. Phase 0 ABI Decision

<!-- txdoc:VDSO-PHASE-0-ABI -->

### 3.1 Static musl TLS ownership

Static musl startup performs:

```text
_start
  -> _start_c
  -> __libc_start_main
  -> __init_libc
  -> __init_tls(aux)
  -> __init_tp(...)
```

`__init_tls(aux)` scans program headers using auxv:

```text
AT_PHDR
AT_PHENT
AT_PHNUM
```

Then it finds `PT_TLS`, computes static TLS layout, allocates memory if needed, copies `.tdata`, zeroes `.tbss`, and initializes `tp`.

Therefore, for static musl v1, the kernel must **not** duplicate this work.

### 3.2 Kernel responsibilities

The kernel must provide:

```text
1. Correct initial user stack:
   argc, argv, envp, auxv.

2. Correct auxv:
   AT_PHDR
   AT_PHENT
   AT_PHNUM
   AT_PAGESZ
   AT_RANDOM
   AT_ENTRY
   later: AT_SYSINFO_EHDR

3. Working anonymous mmap before musl TLS init needs it.

4. Working set_tid_address syscall.

5. Correct preservation of user tp/x4:
   - syscall entry/exit
   - trap return
   - context switch
   - signal frame / sigreturn
```

### 3.3 Kernel non-responsibilities for static musl v1

```text
- Do not parse PT_TLS for required execution.
- Do not allocate initial TLS block.
- Do not compute TP_ADJ.
- Do not set initial tp for musl static startup.
```

`tp` starts with whatever the initial user context provides; musl will set it.

---

## 4. Architecture Placement

vDSO belongs to the ABI / VM special-mapping layer.

```text
exec loader
  -> builds detached AddressSpace
  -> maps ELF LOAD segments
  -> maps stack
  -> maps optional VVAR/VDSO special pages
  -> builds auxv
  -> commits new AddressSpace at PoNR
```

vDSO/VVAR are **derived materializations** of kernel state, not authoritative state. The authoritative state remains in:

```text
timekeeper
scheduler/thread runtime
signal subsystem
VM recipe index
```

The vDSO page is only a user-visible fast path. If vDSO cannot answer, it returns `-ENOSYS` or equivalent, and libc falls back to syscall.

---

## 5. VM Mapping Model

<!-- txdoc:VDSO-VM-SPECIAL-MAPPING -->

### 5.1 New VM special backing

Do **not** map vDSO as `PrivateCow`.

Introduce a VM special mapping kind:

```rust
pub enum VmSpecialKind {
    VdsoText,
    Vvar,
}

pub enum VmBacking {
    // existing variants...
    Special {
        kind: VmSpecialKind,
        /// Number of physically contiguous page frames, allocated and
        /// identity-mapped by the kernel at init time. The frames are
        /// immutable (VdsoText) or kernel-writable (Vvar) after init;
        /// userspace never sees writable mappings.
        num_pages: usize,
        max_prot: Prot,
    },
}
```

Required permissions:

```text
VDSO text:
  user: R | X
  kernel: immutable after init
  forbidden: W

VVAR:
  user: R
  kernel: writable through kernel alias
  forbidden: W | X from userspace
```

### 5.2 Special mapping rules

```text
mprotect(vdso, +W) -> EACCES or EINVAL
mprotect(vvar, +W) -> EACCES or EINVAL
mprotect(vvar, +X) -> EACCES or EINVAL
mremap(vdso/vvar) -> forbidden in v1
munmap(vdso/vvar) -> policy decision:
  v1 recommendation: allow munmap; libc falls back
```

If `munmap` is allowed, future accesses to vDSO simply fail in userspace and libc must use syscall fallback. If stricter behavior is desired, mark the regions `VM_SPECIAL_FIXED` and reject `munmap`. For bringup, allowing `munmap` is simpler.

### 5.3 Address placement

Do not permanently hardcode a magic address.

Add:

```rust
pub fn allocate_special_user_region(
    aspace: &AddressSpace,
    size: usize,
    policy: VmSpecialPlacement,
) -> Result<UserVa, VmMapError>;
```

Recommended v1 layout:

```text
high user VA
  [vvar page]
  [optional guard page]
  [vdso image]
```

The vDSO code may assume VVAR is at a fixed offset relative to `vdso_base`, for example:

```text
vvar_base = vdso_base - PAGE_SIZE
```

But `vdso_base` itself should be chosen by VM placement policy.

---

## 6. VVAR Design

<!-- txdoc:VDSO-VVAR-DESIGN -->

### 6.1 v1: global dedicated VVAR page

Do **not** use per-CPU VVAR in v1.

Per-CPU VVAR requires migration detection, reliable `getcpu`, or rseq-like coordination. Without that, a thread can migrate while still reading a stale CPU-local page.

v1 uses one global page:

```rust
#[repr(C, align(4096))]
pub struct VvarPage {
    pub seq: AtomicU64,

    pub realtime_sec: AtomicU64,
    pub realtime_nsec: AtomicU64,

    pub monotonic_sec: AtomicU64,
    pub monotonic_nsec: AtomicU64,

    pub clock_mode: AtomicU32,
    pub resolution_ns: AtomicU64,

    pub _reserved: [u8; VVAR_RESERVED_SIZE],
}
```

The page must be a **dedicated physical frame**:

```text
- page-aligned
- page-sized
- no unrelated kernel object shares the same physical page
- mapped user-readonly
- updated only through kernel alias
```

### 6.2 Clock quality

Initial VVAR can provide only coarse time if it is updated from timer tick:

```text
CLOCK_REALTIME_COARSE
CLOCK_MONOTONIC_COARSE
```

For high-resolution:

```text
VVAR must publish:
  cycle_last
  mask
  mult
  shift
  realtime_base
  monotonic_base

vDSO must read user-accessible counter:
  e.g. RISC-V time CSR if allowed
```

Until this is stable, `__vdso_clock_gettime` may return `-ENOSYS`.

### 6.3 Seqcount protocol

Writer:

```rust
pub fn update_vvar_time(new: TimeSnapshot) {
    VVAR.seq.fetch_add(1, Ordering::Relaxed); // odd: writer active
    core::sync::atomic::compiler_fence(Ordering::Release);

    VVAR.realtime_sec.store(new.realtime_sec, Ordering::Relaxed);
    VVAR.realtime_nsec.store(new.realtime_nsec, Ordering::Relaxed);
    VVAR.monotonic_sec.store(new.monotonic_sec, Ordering::Relaxed);
    VVAR.monotonic_nsec.store(new.monotonic_nsec, Ordering::Relaxed);

    core::sync::atomic::compiler_fence(Ordering::Release);
    VVAR.seq.fetch_add(1, Ordering::Release); // even: stable
}
```

Reader in vDSO:

```c
do {
    seq1 = load_acquire(vvar->seq);
    if (seq1 & 1) continue;

    sec  = load_relaxed(vvar->monotonic_sec);
    nsec = load_relaxed(vvar->monotonic_nsec);

    seq2 = load_acquire(vvar->seq);
} while (seq1 != seq2);
```

---

## 7. vDSO ELF Design

### 7.1 Build-time crate

Create:

```text
crates/tx-vdso/
  Cargo.toml
  build.rs
  src/lib.rs
  src/vdso.S
  vdso.ld
```

`build.rs` responsibilities:

```text
1. Invoke RISC-V cross compiler / linker.
2. Produce vdso.so.
3. Validate:
   - ELF magic
   - ELFCLASS64
   - RISC-V machine type
   - PT_LOAD exists
   - ELF header is at file offset 0
   - .dynamic is mapped
   - dynsym/dynstr/hash are mapped
   - no unsupported relocations
   - total mapped size is page-aligned and bounded
4. Generate Rust metadata:
   - VDSO_IMAGE
   - VDSO_IMAGE_SIZE
   - VDSO_NUM_PAGES
   - VDSO_VVAR_REL_OFFSET
```

The kernel runtime should **not** use `goblin` or a full ELF parser.

### 7.2 Runtime kernel view

In `crates/tx-kernel/src/vdso/mod.rs`:

```rust
pub struct KernelVdso {
    pub image: &'static [u8],
    pub image_size: usize,
    pub num_pages: usize,
    pub frames: &'static [/* page-allocator frame handle */],
    pub vvar_relative_offset: isize,
}

pub fn init_vdso() -> Result<(), VdsoInitError>;
pub fn kernel_vdso() -> &'static KernelVdso;
```

`init_vdso()`:

```text
1. Allocates dedicated frames for VDSO image.
2. Copies prevalidated VDSO bytes into frames.
3. Marks frames immutable / executable in kernel metadata.
4. Stores KernelVdso singleton.
```

### 7.3 ELF header rule

The ELF header must be mapped at:

```text
vdso_base + 0
```

Therefore:

```text
AT_SYSINFO_EHDR = vdso_base
```

Do not support `ehdr_offset != 0` in v1.

### 7.4 Required symbols

Initial stub vDSO exports:

```text
__vdso_clock_gettime
__vdso_gettimeofday
__vdso_clock_getres
```

Optional later:

```text
__vdso_rt_sigreturn
__vdso_getcpu
```

v1 behavior:

```text
__vdso_clock_gettime:
  return -ENOSYS unless supported clock mode is enabled

__vdso_gettimeofday:
  return -ENOSYS or use same coarse VVAR path

__vdso_clock_getres:
  may return static coarse resolution
```

### 7.5 RISC-V assembly ABI constraints

vDSO assembly must:

```text
- use RISC-V calling convention
- preserve callee-saved registers
- not clobber tp/x4
- be PIC or PC-relative
- not require runtime dynamic relocation
- locate VVAR through fixed relative offset from vdso_base
```

Prototype:

```c
int __vdso_clock_gettime(clockid_t clk_id, struct timespec *ts);
int __vdso_gettimeofday(struct timeval *tv, struct timezone *tz);
int __vdso_clock_getres(clockid_t clk_id, struct timespec *res);
```

---

## 8. Exec Integration

<!-- txdoc:VDSO-EXEC-INTEGRATION -->

### 8.1 `ExecImagePlan`

Minimum fields required for musl-static bootstrap (actual type in
`crates/tx-scripts/src/process/exec/loader.rs` carries additional
fields — `stack_top`, `bss_extension`, `at_phnum` — consumed by sibling
phases; this section lists only the musl-bootstrap-relevant subset):

```rust
pub struct ExecImagePlan {
    pub image_kind: ExecImageKind, // ET_EXEC or ET_DYN
    pub load_bias: u64,
    pub entry: UserVa,

    pub phdr_vaddr: UserVa,
    pub phent: u64,
    pub phnum: u64,

    pub load_segments: Vec<LoadSegmentPlan>,
}
```

`PT_TLS` parsing is optional and not required for v1 static musl.

### 8.2 Auxv facts

```rust
pub struct AuxvFacts {
    pub at_phdr: u64,
    pub at_phent: u64,
    pub at_phnum: u64,
    pub at_pagesz: u64,
    pub at_entry: u64,
    pub at_random: u64,

    pub at_sysinfo_ehdr: Option<u64>,
}
```

Auxv emission:

```text
AT_PHDR          = facts.at_phdr
AT_PHENT         = facts.at_phent
AT_PHNUM         = facts.at_phnum
AT_PAGESZ        = PAGE_SIZE
AT_ENTRY         = facts.at_entry
AT_RANDOM        = user pointer to 16 random bytes on initial stack
AT_SYSINFO_EHDR  = vdso_base, if vDSO mapped
AT_NULL          = 0
```

### 8.3 Static PIE rules

For `ET_DYN` static PIE:

```text
load_bias = chosen_base - lowest_load_vaddr
AT_ENTRY  = load_bias + e_entry
AT_PHDR   = load_bias + PT_PHDR.p_vaddr
```

If no `PT_PHDR` exists, compute:

```text
AT_PHDR = load_bias + first_load_segment_runtime_base + (e_phoff - first_load.p_offset)
```

But v1 recommendation is:

```text
Require PT_PHDR for static PIE during bringup.
Reject malformed images early with ENOEXEC.
```

For `ET_EXEC`:

```text
load_bias = 0
AT_ENTRY = e_entry
AT_PHDR = runtime address of program header table
```

### 8.4 `map_vdso_into_aspace`

```rust
pub fn map_vdso_into_aspace<P: PmapIf>(
    aspace: &AddressSpace,
) -> Result<VdsoMapping, VmMapError>;

pub struct VdsoMapping {
    pub vdso_base: UserVa,
    pub vdso_size: usize,
    pub vvar_base: UserVa,
}
```

Implementation:

```text
1. Reserve high user VA region.
2. Install VVAR VmEntry:
   - backing = VmBacking::Special { Vvar }
   - prot = R
   - max_prot = R
3. Install VDSO VmEntry:
   - backing = VmBacking::Special { VdsoText }
   - prot = R | X
   - max_prot = R | X
4. Return vdso_base and vvar_base.
```

### 8.5 PoNR discipline

Before exec point-of-no-return:

```text
- parse ELF
- create detached AddressSpace
- map ELF LOAD segments
- map stack
- map optional VVAR/VDSO
- build auxv
- build initial register context
```

After PoNR:

```text
- swap process AddressSpace
- install saved user context
- reset signal dispositions
- apply close-on-exec
- publish exec trace
```

Any failure in VDSO/VVAR mapping must occur before PoNR.

---

## 9. Syscall Requirements Before vDSO

Static musl self-init requires these before real vDSO matters:

```text
mmap
munmap, if musl cleanup path uses it
brk, if allocator path falls back to brk
set_tid_address
clock_gettime syscall fallback
write / exit for diagnostics
```

`set_tid_address` minimal implementation:

```rust
pub fn sys_set_tid_address(tidptr: UserPtr<i32>) -> Result<Pid, Errno> {
    current_thread().clear_child_tid.store(Some(tidptr));
    Ok(current_thread().tid())
}
```

For v1, clear-child-tid futex wake may be stubbed if pthread exit is not yet supported, but the pointer must be stored and the syscall must return the current tid.

---

## 10. User `tp` Preservation

RISC-V `tp` is x4. Kernel must preserve it as part of user register state.

Required fields:

```rust
pub struct UserTrapFrame {
    // ...
    pub x4_tp: usize,
    // ...
}
```

Required preservation points:

```text
1. syscall entry saves x4/tp
2. syscall return restores x4/tp
3. timer interrupt saves/restores x4/tp
4. context switch preserves saved user trap frame
5. signal frame includes tp in ucontext
6. sigreturn restores tp
```

Kernel must not interpret musl `TP_ADJ`.

---

## 11. Phase Plan

<!-- txdoc:VDSO-PHASE-PLAN -->

### Phase 1 — musl-static bootstrap ABI

Goal:

```text
Run static musl programs that do not require vDSO.
```

Work:

```text
- Fix auxv generation.
- Ensure AT_PHDR is runtime virtual address.
- Implement AT_RANDOM stack allocation.
- Implement set_tid_address.
- Verify mmap works before libc TLS init.
- Ensure tp is preserved across traps.
```

Validation:

```c
#include <unistd.h>

int main() {
    write(1, "hello\n", 6);
    return 0;
}
```

And auxv dumper:

```text
- AT_PHDR points to mapped memory
- AT_PHENT == sizeof(Elf64_Phdr)
- AT_PHNUM == ELF e_phnum
- AT_PAGESZ == 4096
- AT_RANDOM points to readable 16 bytes
```

---

### Phase 2 — TLS self-init validation

Goal:

```text
Prove static musl initializes TLS without kernel-built TLS.
```

Test:

```c
__thread int counter = 42;

int main() {
    return counter == 42 ? 0 : 1;
}
```

Validation:

```text
- Program exits 0.
- set_tid_address is observed.
- tp changes from initial value to musl-computed value.
- tp survives syscall round trips.
- mmap path is exercised if TLS allocation exceeds builtin TLS.
```

Optional stress test:

```c
__thread char big_tls[65536] = {1};

int main() {
    return big_tls[0] == 1 ? 0 : 1;
}
```

---

### Phase 3 — empty vDSO ELF mapping

Goal:

```text
Expose a valid vDSO ELF through AT_SYSINFO_EHDR.
```

Work:

```text
- Add tx-vdso crate.
- Build minimal RISC-V vDSO ELF.
- Export stub symbols.
- Generate build-time metadata.
- Map VDSO as VmBacking::Special(VdsoText).
- Add AT_SYSINFO_EHDR.
```

Stub behavior:

```text
__vdso_clock_gettime -> -ENOSYS
__vdso_gettimeofday  -> -ENOSYS
__vdso_clock_getres   -> valid coarse result or -ENOSYS
```

Validation:

```text
- getauxval(AT_SYSINFO_EHDR) returns nonzero.
- Address points to ELF magic.
- libc still works through syscall fallback.
- clock_gettime succeeds through syscall fallback.
```

---

### Phase 4 — global VVAR page

Goal:

```text
Map a user-readonly VVAR page and update it safely.
```

Work:

```text
- Allocate dedicated global VVAR frame.
- Add kernel writable alias.
- Map VVAR next to vDSO.
- Add seqcount update path.
- Initially publish coarse realtime/monotonic.
```

Validation:

```text
- User can read VVAR.
- User cannot write VVAR.
- User cannot execute VVAR.
- Seqcount remains even outside updates.
- Time values are non-decreasing.
```

---

### Phase 5 — coarse vDSO time

Goal:

```text
Make __vdso_clock_gettime work for coarse clocks.
```

Supported:

```text
CLOCK_REALTIME_COARSE
CLOCK_MONOTONIC_COARSE
```

Unsupported:

```text
CLOCK_REALTIME          -> -ENOSYS
CLOCK_MONOTONIC         -> -ENOSYS, unless coarse semantics are explicitly accepted
CLOCK_MONOTONIC_RAW     -> -ENOSYS
```

Validation:

```text
- Coarse clock returns 0.
- Unsupported clock falls back to syscall.
- No kernel trap occurs for supported coarse vDSO call.
```

---

### Phase 6 — high-resolution vDSO time

Goal:

```text
Support CLOCK_REALTIME and CLOCK_MONOTONIC from vDSO.
```

Prerequisites:

```text
- stable RISC-V user-readable counter
- scounteren configured if needed
- invariant counter across harts, or migration-safe fallback
- timekeeper publishes cycle conversion fields
```

Work:

```text
- Extend VVAR with cycle_last/mult/shift/mask.
- Add clock_mode.
- vDSO reads counter.
- vDSO computes delta ns.
- Normalize sec/nsec.
```

Validation:

```text
- clock_gettime(CLOCK_MONOTONIC) uses no syscall.
- Returned values are non-decreasing.
- Cross-CPU migration does not regress time.
- Fallback occurs if clock_mode unsupported.
```

---

### Phase 7 — optional signal trampoline in vDSO

Goal:

```text
Move rt_sigreturn trampoline from user stack to vDSO.
```

Work:

```text
- Export arch-specific sigreturn symbol.
- Signal frame return_pc points to vDSO trampoline.
- Trampoline performs rt_sigreturn syscall.
```

Validation:

```text
- Signal handler returns correctly.
- sigreturn restores full user context, including tp.
- User stack does not need executable trampoline.
```

---

## 12. Test Matrix

| Test                    | Required phase | Expected result                        |
| ----------------------- | -------------: | -------------------------------------- |
| hello static musl       |        Phase 1 | exits 0                                |
| auxv dumper             |        Phase 1 | correct PHDR/PHENT/PHNUM/PAGESZ/RANDOM |
| `__thread int x=42`     |        Phase 2 | exits 0                                |
| large TLS object        |        Phase 2 | exercises mmap, exits 0                |
| `set_tid_address` trace |        Phase 2 | syscall observed, tid returned         |
| `tp` preservation test  |        Phase 2 | same tp before/after syscall           |
| getauxval VDSO          |        Phase 3 | valid ELF header                       |
| clock_gettime fallback  |        Phase 3 | succeeds through syscall               |
| VVAR permission test    |        Phase 4 | read ok, write/exec fault              |
| coarse clock vDSO       |        Phase 5 | no syscall for supported coarse clocks |
| high-res clock vDSO     |        Phase 6 | no syscall, monotonic                  |
| signal handler return   |        Phase 7 | handler returns, tp restored           |

---

## 13. Failure Policy

Before exec PoNR:

```text
VDSO/VVAR map failure -> execve returns error.
auxv build failure    -> execve returns error.
stack build failure   -> execve returns error.
```

After exec PoNR:

```text
No recoverable allocation failure should occur.
If fatal corruption is detected, terminate process.
```

vDSO runtime failure:

```text
Unsupported clock -> -ENOSYS
Malformed VVAR seq loop -> retry boundedly, then -ENOSYS
User pointer invalid -> return -EFAULT or fallback syscall, depending on libc ABI expectation
```

---

## 14. Open Questions

<!-- txdoc:VDSO-OPEN-QUESTIONS -->

```text
1. Should munmap(vdso/vvar) be allowed in v1?
   Resolved: allow, rely on libc fallback.

2. Should AT_SYSINFO_EHDR be omitted until vDSO ELF is valid?
   Resolved: yes. Do not expose a fake pointer.

3. Should coarse CLOCK_MONOTONIC be returned for CLOCK_MONOTONIC?
   Resolved: no. Return -ENOSYS until high-res path is correct.

4. Should kernel parse PT_TLS for diagnostics?
   Resolved: optional debug-only parser outside critical exec path.

5. Should VVAR be global forever?
   Resolved: global in v1; per-CPU only after getcpu/rseq/migration checks exist.
```

---

## 15. Final Implementation Order

```text
1. Auxv correctness
2. mmap + set_tid_address + tp preservation
3. TLS self-init tests
4. Minimal vDSO ELF mapping
5. Global VVAR page
6. Coarse vDSO clock
7. High-resolution vDSO clock
8. vDSO sigreturn trampoline
```

The important priority inversion is:

```text
old:
  VDSO -> kernel TLS -> auxv validation

new:
  auxv + mmap + set_tid_address + tp preservation
    -> musl TLS self-init validation
    -> empty vDSO ELF
    -> real vDSO clock
```

This makes vDSO a performance and ABI-completeness layer, not a blocker for static musl bringup.
