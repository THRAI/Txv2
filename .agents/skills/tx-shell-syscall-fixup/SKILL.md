---
name: tx-shell-syscall-fixup
description: Use when a userspace shell or program (busybox, musl-linked binary, etc.) misbehaves under txKernel — wrong return value, ENOSYS, SIGSEGV downstream — and needs a tight observe→fix→verify loop. Pairs the trap-trace + fault-decode + shell-test xtask tools with a syscall-stub edit pattern.
---

# tx-shell-syscall-fixup

Use when:

- A user binary (busybox, init, an LTP program, etc.) crashes or
  hangs under QEMU and you suspect a syscall stub is missing,
  returning the wrong value, or corrupting userspace state.
- An interactive shell test under `cargo xtask shell-test` fails
  an `expect` directive and you need to find which kernel-side
  syscall caused the divergence.
- You're adding new busybox commands and want to know which
  syscall implementations they need before stubbing blindly.

Do **not** use this skill for kernel-internal debugging
(reschedule longjmp, page fault handlers, VM scripts) — those are
covered by `tx-hal-axhal`, `tx-vm-pagebacked`, and the
`fault-decode` tool. This skill assumes the kernel reaches the
userspace round-trip and the question is "what does userspace
actually see in registers?".

## Read First

- `docs/DEVELOPMENT.md` — "Debug tooling" section pins the
  contract for `fault-decode`, `trap-trace`, and `shell-test`.
- `boards/tx-hal-riscv64-qemu-virt/src/debug_trace.rs` — the
  `txdbg:trap` / `txdbg:ent` wire format the parser consumes.
- `crates/tx-shims/src/linux_syscall/numbers.rs` — current syscall
  NR table with mnemonics and shape pins.
- `crates/tx-shims/src/linux_syscall/mod.rs` — syscall dispatcher
  + existing `sys_*` implementations to model new ones on.
- `tools/shell-tests/busybox-prompt.txt` — sample shell-test
  script with the explicit-pause discipline.

## The Loop

One iteration is small: observe one bad behaviour, identify the
guilty syscall, ship the fix, verify with the same script.

### 1. Reproduce with `--trap-trace`

Build the kernel with the gated trap-trace feature on and run the
failing script (or the existing busybox-smoke):

```sh
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf --features trap-trace
cargo xtask shell-test --target rv64-qemu --script tools/shell-tests/YOUR_SCRIPT.txt
# or, for non-interactive:
cargo xtask test busybox-smoke --target rv64-qemu --trap-trace
```

The kernel emits `txdbg:trap` and `txdbg:ent` records to the
serial log. Grab the log path from the run output (typically
`target/qemu-rv64-qemu-busybox.serial.log` or similar).

### 2. Identify the divergence

Use `cargo xtask trap-trace --serial PATH --syscalls` to get a
human-readable timeline:

```text
[0x003e] SY pc=0x...  openat                   a7=0x38  ...  -> -2 (errno ENOENT)
[0x004a] SY pc=0x...  writev                   a7=0x42  ...  -> -38 (errno ENOSYS)   ← divergence
[0x004b] lPF pc=0x...  stval=0x746f672e00617461                                       ← downstream crash
```

Look for:

- **`-38 (errno ENOSYS)`** — the dispatcher hit the unknown-NR
  arm. Action: implement a stub. NR mnemonic comes from the
  parser's hardcoded RV64-generic table.
- **Wrong return value** (e.g. `getcwd` returns `2` for `"/"` —
  that's correct, len incl. NUL; but `0` would be a bug).
- **A page fault at user PC that follows a suspicious return**
  (the classic pattern: bad syscall return → userspace
  dereferences as a pointer → SIGSEGV).
- **Repeated identical syscall** (same `pc`, same args) — sign
  the kernel isn't advancing `sepc` past `ecall`, OR busybox is
  retrying because the return value indicates "would block".

### 3. Pinpoint the user-side context (optional, for SIGSEGV)

When the divergence is a page fault, decode it:

```sh
cargo xtask fault-decode --target rv64-qemu --serial PATH
```

For a faulting user PC, also disassemble the offending
instruction:

```sh
rust-objdump -d --start-address=0xCC2D8 --stop-address=0xCC2F0 \
  tools/images/vendor/busybox-riscv64-musl
```

The instruction class (load/store/branch) plus the register state
in the trap dump tells you which input was bad. If a register
holds an obviously-non-pointer value (ASCII bytes, decimal
repetitions, all-zeros), trace it back to the most recent
syscall return.

### 4. Implement or fix the stub

Add the syscall in `crates/tx-shims/src/linux_syscall/`:

- **Number constant** in `numbers.rs` with a doc comment that
  cites the Linux ABI table and the busybox-side use case
  (e.g. "musl stdio uses writev for buffered output").
- **Dispatch arm** in `mod.rs`'s match — keep the table
  alphabetised within an existing block where possible.
- **`sys_*` impl** modelled on the closest existing syscall.
  For minimal stubs, a return-fixed-value or wrap-an-existing-
  syscall pattern is acceptable; mark explicit limitations in the
  doc comment so a future workload that hits them surfaces a
  clear signal.

Patterns from this branch:

- **`sys_writev` / `sys_readv`** — iovec loop over `sys_write` /
  `sys_read`. Linux partial-success semantics: short or failed
  entry N returns running total if `total > 0`.
- **`sys_ppoll`** — minimal stub that marks all fds "ready" and
  returns `nfds`. busybox blocks in the follow-up `read()`
  instead of `ppoll()`. Doc the timeout-ignored limitation.

### 5. Add or update a parser entry

If the new NR isn't in `xtask/src/trap_trace.rs::rv64_generic_syscall_names()`,
add it. The mnemonic should match `numbers.rs` exactly.

### 6. Add or extend a shell-test script

Each fix should be defended by an explicit test in
`tools/shell-tests/`. Format:

```text
# Why this exists: <busybox command, kernel behaviour under test>
wait "/ # " within 10000
sleep 500              # heisenbug catch — explicit pause before any input
send "<command>\n"
expect "<expected substring>" within 5000
sleep 500
quit
```

Always include `sleep` before each `send`. The driver does NOT
silently coalesce input; the explicit pause forces timing
diversity so timing-dependent bugs surface in test, not in prod.

### 7. Verify

```sh
# Same script, kernel without trap-trace (production-shape build)
cargo xtask shell-test --target rv64-qemu --script tools/shell-tests/YOUR_SCRIPT.txt
# Workspace tests still green
cargo test --workspace --lib --tests -- --test-threads=1
# Existing smoke still passes
cargo xtask test busybox-smoke --target rv64-qemu
```

## Rules

- **Do not invent NRs.** Source of truth is Linux `asm-generic/unistd.h`.
  RV64 generic ABI numbers are mirrored in `numbers.rs` and the
  parser table.
- **Stub limitations belong in the doc comment.** If `sys_X`
  ignores a flag, returns a fixed value, or doesn't honour
  timeouts, say so out loud. The next workload to hit the
  limitation should know what's not implemented.
- **Errno values come from `numbers.rs`'s constants.** Don't
  hard-code negative numbers; use `EINVAL_VALUE`, `EBADF_VALUE`,
  `ENOSYS_VALUE`, etc., so changes ripple cleanly.
- **No silent stubs.** If a syscall accepts arguments the stub
  ignores, the impl should reject those args with `-EINVAL`
  rather than pretending it handled them. "Pretend success" is
  the worst failure mode — userspace continues with wrong state.
- **Trap-trace is debug-only.** The `trap-trace` feature is off
  by default and must stay so for production / CI builds. The
  default `cargo xtask test busybox-smoke` run emits zero
  `txdbg:` records; verify after every change.
- **Pair every new sys_* with a shell-test.** A syscall without a
  test will regress silently the moment busybox or musl shifts
  its call pattern.
- **Don't merge instrumentation hacks.** Any one-shot
  instrumentation added to `dispatch_trap_frame` or
  `enter_userspace_with_context` for a debugging session must be
  reverted before the commit lands. Use the gated `record_trap` /
  `record_entry` infrastructure instead — extend it if a new
  record kind is needed (and update the parser at the same time).

## Done Means

- The failing script's `expect` directives all pass on a
  default-features kernel build.
- Workspace host tests are green.
- `cargo xtask test busybox-smoke` (sentinel-watch lane) still
  passes.
- The new syscall has:
  - A `numbers.rs` entry with NR + doc comment.
  - A dispatcher arm.
  - A `sys_*` impl with limitations doc'd.
  - A parser entry in `rv64_generic_syscall_names()`.
  - A shell-test script under `tools/shell-tests/` with explicit
    pre-input pauses.
- The commit message names the syscall, why busybox needs it, and
  the script that defends it.

## Anti-patterns

- **Implementing a syscall before observing the failure.**
  Without trap-trace evidence you don't know which NR busybox
  actually wants — the assumed list and the real list often
  diverge (e.g. busybox's musl uses `writev` not `write` for
  stdio).
- **Skipping the explicit-sleep discipline.** A test that
  burst-sends input after the prompt is a test that masks
  timing bugs. The cost of a 500 ms pause per directive is
  measured in seconds, not minutes.
- **Adding new `[u N ...]` ad-hoc trace dumps to
  `dispatch_trap_frame`.** That pattern was retired in
  `1a41c08`. Use `record_trap` / `record_entry` (the gated
  helpers in `boards/tx-hal-riscv64-qemu-virt/src/debug_trace.rs`)
  and extend the parser to read whatever new keys you emit. The
  format is grep-stable — don't break existing kinds.
- **Coalescing input into a single `send`.** One directive per
  user gesture. The driver mirrors keystrokes; merging them
  changes timing semantics.
