# Direct-I/O completion wait/result (2026-07-14)

## Change

`PageContainer` now offers waitable direct-read and direct-write admission in
addition to the compatibility submission APIs. A waitable submission carries
the neutral `DirectIoSubmission` used by the filesystem planner plus the
terminal `PageReadyWait` endpoint. The PageContainer retains the wait source
until the caller consumes the matching result.

At terminal completion, PageBacked removes the in-flight lease, applies the
existing range/cache coherency operation, drops the DMA buffer, records the
success or error result, and notifies the wait source outside its state lock.
The old `submit_file_direct_read` and `submit_file_direct_write` APIs retain no
wait source and create no completed-result row.

## Boundary

This is the asynchronous direct-I/O result boundary needed by a future syscall
StepOp. It does not enqueue a direct bio itself, register ext4 file containers
with a device runtime, parse `O_DIRECT`, implement `msync` or `syncfs`, or make
a JBD2 durability claim.
