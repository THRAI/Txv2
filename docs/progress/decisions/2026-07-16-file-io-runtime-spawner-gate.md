# File I/O Runtime Spawner Gate

Date: 2026-07-16

## Canonical Gate

| Proposed item | Canonical source | Live seam | Decision |
| --- | --- | --- | --- |
| `FileIoServiceRuntimeSpawner` | `IO_MANAGER_v1.md` service futures: the reactor owns long-lived service futures; `DEVICE.md` keeps block devices static bindings | `device::register_page_container_file_io_service` records PageContainer service runtime; `CoreInit::submit_file_io_runtime_tasks` currently consumes only one boot snapshot | Accept in `tx-subsystems::device` as an inversion-of-control boundary. It owns no reactor state. |
| Exactly-once runtime claim | `IO_MANAGER_v1.md` queue/service ownership; `PAGE_BACKED_v1.md` L4 owns PageContainer request handling | registry currently clones every runtime into boot submission without a claimed state | Accept: registry marks a runtime submitted before calling the spawner; no registry/spawner lock crosses the call. |
| Kernel reactor implementation | `IO_MANAGER_v1.md` says services are reactor futures; `DEVICE.md` makes the kernel responsible for device execution scheduling | `CoreInit` owns `BOOT_REACTOR` and already submits the file-I/O service loop | Deferred to the next commit. Kernel installs the one spawner after the boot reactor exists. |
| Regular RW ext4 `mount(2)` conversion | `TX_EXT4_PLAN_v1_2.md` ordered journaling and `PAGE_BACKED_v1.md` L4 fsync routing | `linux_syscall/fs_mut.rs` still calls the compatibility `mount_ext4_read_write` path | Deferred until the kernel spawner is installed and covered by an exact-once test. |

## Consequences

- `tx-subsystems` never imports `tx-reactor` or `tx-kernel`.
- A runtime registered before boot is drained when the kernel installs its spawner.
- A runtime registered later is submitted by the same claim path, once.
- The claim is intentionally irreversible: the kernel installs its spawner only after `BOOT_REACTOR` is initialized, so retry ownership is not ambiguous.
