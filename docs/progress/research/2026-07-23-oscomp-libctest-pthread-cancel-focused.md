# 2026-07-23 OSComp libctest-musl pthread-cancel focused run

## What changed

- PageBacked file-I/O service submission planning now reuses an active epoch
  guard instead of unconditionally opening a nested guard.
- `Published<RecipeTree>` commit now performs one full epoch drain and retries
  before treating an occupied retire bag as fatal.

## Verification

- `cargo test -p tx-substrate --test publication commit_recovers_when_prior_retire_bags_need_full_drain -- --nocapture`
- `cargo test -p tx-subsystems file_page_container_drives_service_submission_under_existing_epoch_guard --lib -- --nocapture`
- `cargo test -p tx-kernel thread_future --lib -- --nocapture`
- `rustfmt --check crates/tx-substrate/src/publication/mod.rs crates/tx-substrate/tests/publication.rs crates/tx-subsystems/src/page_backed/mod.rs crates/tx-subsystems/src/page_backed/core_tests.rs`
- `git diff --check -- crates/tx-substrate/src/publication/mod.rs crates/tx-substrate/tests/publication.rs crates/tx-subsystems/src/page_backed/mod.rs crates/tx-subsystems/src/page_backed/core_tests.rs`
- `cargo xtask progress validate`

## Guest evidence

Built a focused 256 MiB RV64 sdcard at:

```text
target/oscomp/libctest-pthread-cancel-focus/sdcard-rv.img
```

Production-shape filtered boot:

```sh
cargo xtask oscomp test --target rv64-qemu \
  --data target/oscomp/libctest-pthread-cancel-focus \
  --submit target/oscomp/submit \
  --suite libctest-musl \
  --boot-suite 'libctest-musl:pthread_cancel_points+pthread_cancel+pthread_cancel_sem_wait+pthread_exit_cancel'
```

The QEMU wrapper did not exit after suite output, so the run was collected with
an outer timeout. Saved serial:

```text
target/oscomp/os_serial_out_rv.txt
```

Observed results:

- static `pthread_cancel`: pass
- static `pthread_cancel_sem_wait`: pass
- static `pthread_exit_cancel`: pass
- static `pthread_cancel_points`: fail at `shm_open` with
  `res != PTHREAD_CANCELED`
- dynamic `pthread_cancel*`: fail before test logic with
  `entry-dynamic.exe exec failed` in the slim image
- `cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_rv.txt --all --brief`
  reports no trap lines

Manual score over the saved serial reports `libctest-musl 3/220`; this score is
not a full-suite score because the filtered boot intentionally ran only selected
cases while the stock judge table still expects all libctest rows.

## Next step

Trace `pthread_cancel_points` with a trap-trace build focused on
`libctest-musl:static:pthread_cancel_points`, then decide whether the
remaining failure belongs to POSIX shm/open cancellation-point semantics,
`pthread_setcancelstate` pending-cancel behavior, or `/dev/shm` setup in the
test-init/slim-image path.

## Follow-up: namespace open/unlink and full libctest-musl wrapper run

After adding namespace-aware `openat(O_CREAT)` and namespace-aware
`unlinkat` parent walking, the focused static pthread-cancel selectors pass:

- `pthread_cancel_points`
- `pthread_cancel`
- `pthread_cancel_sem_wait`
- `pthread_exit_cancel`

The first full `libctest-musl` attempt with
`tx.oscomp.groups=libctest-musl` was invalid because `/tx-test-init` executed
the kernel-provided full libctest command through `/bin/sh -c "$payload"`.
That made the guest shell receive one huge argv entry and fail with:

```text
/tx-test-init: line 195: /bin/sh: Argument list too long
```

Saved invalid serial:

```text
target/oscomp/libctest-pthread-cancel-focus/libctest-musl-full-invalid-arg-list-after-namespace-open-unlink.txt
```

The wrapper now writes the payload into `/tmp/tx-test-payload.sh` and executes
that script with `/bin/sh`, avoiding the second exec's argv-size limit. A
marker-only QEMU run then reached:

```text
#### OS COMP TEST GROUP START libctest-musl ####
```

The bounded full run reached:

```text
#### OS COMP TEST GROUP END libctest-musl ####
```

Saved full serial:

```text
target/oscomp/libctest-pthread-cancel-focus/libctest-musl-full-after-test-init-payload-file.txt
```

Judge result:

```text
[libctest-musl] 109/220
总分: 109/220
```

Observed blocker split:

- 109 pass markers.
- 111 fail markers.
- 108 failures are dynamic `entry-dynamic.exe exec failed: No such file or
  directory` in this slim image.
- First real non-dynamic-loader failure is static `stat`.
- `cargo xtask fault-decode --target rv64-qemu --serial
  target/oscomp/os_serial_out_rv.txt --all --brief` reports no trap lines.

Next step: either rebuild/extend the slim image to include the dynamic loader
and dependent DSOs for `entry-dynamic.exe`, or focus static `stat` if the goal
is to raise the currently runnable static score first.
