# LTP Runtest ipc Progress

This tracks the native LTP `runtest/ipc` module. It is not the Txv2
`syscalls` IPC batch; this module is a pipe I/O stress list using `pipeio`.

Run command:

```bash
timeout 600s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ipc
```

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| module entries | 6 | from `make ltp-runtest-cases LTP_RUNTEST=ipc` |
| latest local run | completed | 2026-05-26 |
| local judge score | `0/0` | current `oscomp-judge.py` does not score these old-style detail-only runtest entries |
| observed detail score | `4/38` | TPASS over TPASS+TFAIL+TBROK+TCONF+TWARN from serial detail lines |
| reached case | `pipeio_8` | module completed |
| log | `target/oscomp/ltp-runtests/ipc/os_serial_out_rv.txt` | copied serial snapshot |

## Notes

- `pipeio_3` and `pipeio_4` expose pipe data corruption: `FAIL data error on
  byte 8`, followed by child-exit timeout and repeated unexpected `SIGPIPE`.
- Named-pipe blocking cases (`pipeio_1`, `pipeio_5`) functionally pass their
  read count but return warning status due to `256 empty reads`.
- System-pipe smaller stress cases (`pipeio_6`, `pipeio_8`) pass cleanly.

## Cases

| Entry | Command | Detail | Status | Note |
| --- | --- | ---: | --- | --- |
| `pipeio_1` | `pipeio -T pipeio_1 -c 5 -s 4090 -i 100 -b -f x80` | 1 pass / 1 warn | partial | named pipe blocking read completes, but reports `256 empty reads`; ret=4 |
| `pipeio_3` | `pipeio -T pipeio_3 -c 5 -s 4090 -i 100 -u -b -f x80` | 0 pass / 18 non-pass | fail | system pipe data error at byte 8, child wait timeout, unexpected SIGPIPE; ret=5 |
| `pipeio_4` | `pipeio -T pipeio_4 -c 5 -s 4090 -i 100 -u -f x80` | 0 pass / 18 non-pass | fail | same data corruption / SIGPIPE pattern as `pipeio_3`; ret=5 |
| `pipeio_5` | `pipeio -T pipeio_5 -c 5 -s 5000 -i 10 -b -f x80` | 1 pass / 1 warn | partial | adjusted I/O size to 4096 and completed, but reports `256 empty reads`; ret=4 |
| `pipeio_6` | `pipeio -T pipeio_6 -c 5 -s 5000 -i 10 -b -u -f x80` | 1 pass / 0 non-pass | pass | system pipe blocking case passed; ret=0 |
| `pipeio_8` | `pipeio -T pipeio_8 -c 5 -s 5000 -i 10 -u -f x80` | 1 pass / 0 non-pass | pass | system pipe case passed; ret=0 |
