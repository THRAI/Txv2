# LTP aio Progress

`aio` batch local tracking. Cases are from `tools/ltp-batches.py --batch aio`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 15 | from `make ltp-batch-cases LTP_BATCH=aio` |
| latest local run | `[ltp-musl] 1/6` | 2026-05-26 latest 5-case group |
| cumulative scored | `1/16` | recorded rows in this document |
| reached case | `io_uring02` | batch completed |
| logs | `target/oscomp/ltp-progress/aio` | per-group stdout and serial snapshots |

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
| `io_uring02` | 0/1 | fail | TBROK: chroot(test_root) failed: ENOSYS (38) |
