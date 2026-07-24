# Direct-I/O service wake binding (2026-07-14)

## Change

PageBacked can now attach a neutral `ServiceWakeSource` to a corresponding
PageContainer. After a direct-I/O submission successfully enters the owned L6
queue, PageBacked clones that handle under its short state lock and posts an
`IoServiceKind::Block` kick outside the lock. PageBacked stores no concrete
device handle or driver reference.

## Evidence

- Direct L4-to-L6 test attaches a block-service source, subscribes a mailbox,
  and observes a `SourceFired` event on enqueue.
- `cargo test -p tx-subsystems --lib page_backed --no-default-features -- --nocapture --test-threads=1`: 135 passed.
- `cargo test -p tx-fs --lib bdevfs::tests::materialised_block_device_registers_one_file_io_service_runtime --no-default-features -- --nocapture --test-threads=1`: passed.

## Boundary

The device runtime is responsible for installing this attachment during its
own registration. That runtime integration remains in a separate device-owned
slice. This does not provide a completion wait/result to a caller, make ext4
file PCs register a device runtime, or expose an `O_DIRECT` syscall.
