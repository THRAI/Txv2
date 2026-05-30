# LTP aio Progress

`aio` batch local tracking. Cases are from `tools/ltp-batches.py --batch aio`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 15 | from `make ltp-batch-cases LTP_BATCH=aio` |
| latest local run | full-image aio batch | 2026-05-30 direct QEMU run scored `1/16` |
| cumulative scored | `1/16` | fresh full-image batch score |
| reached case | `io_uring02` | batch completed |
| logs | `target/oscomp/os_serial_out_ltp_aio_full_20260530_191300.txt` | latest full-batch serial snapshot |

## 2026-05-30 full-image run

Direct non-Docker QEMU coverage completed the 15-case `ltp-batch:aio` batch,
scoring `1/16`. The guest exited cleanly and fault decode found no kernel trap
lines. The serial snapshot is
`target/oscomp/os_serial_out_ltp_aio_full_20260530_191300.txt`.

Most raw AIO LTP cases are still harness/configuration-gated rather than
exercising the raw AIO implementation: the `*01`/`*02` variants either require
`CONFIG_AIO=y` from `/proc/config` or require libaio development packages in
the image. `io_uring01` reaches real kernel behavior: `io_uring_setup()`
passes, then the user-ring `mmap()` fails with `EINVAL`. `io_uring02` is
blocked by `capget` probing.

Verified with:
`timeout 120s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:aio`;
`python3 tools/oscomp-judge.py target/oscomp/os_serial_out_ltp_aio_full_20260530_191300.txt target/oscomp/testdata`;
`cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_ltp_aio_full_20260530_191300.txt --all --brief`.

## 2026-05-26 failure notes

- TCONF: 13 recorded case(s); see per-case notes below.
- TBROK: 2 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `io_cancel01` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_cancel02` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_destroy01` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_destroy02` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_getevents01` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_getevents02` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_pgetevents01` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_pgetevents02` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_setup01` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_setup02` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_submit01` | 0/1 | skip | TCONF: test requires libaio and it's development packages |
| `io_submit02` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_submit03` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `io_uring01` | 1/2 | partial | TBROK: mmap(0,0,PROT_READ / PROT_WRITE(3),32769,3,0) failed: EINVAL (22) |
| `io_uring02` | 0/1 | skip | 2026-05-30 full-image run: blocked by unsupported `capget` capability probing |
