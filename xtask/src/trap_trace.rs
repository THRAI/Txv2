//! `cargo xtask trap-trace --serial PATH` — parser + summariser for
//! the `txdbg:trap` / `txdbg:ent` log records emitted by the board's
//! `debug_trace` module under the `trap-trace` cargo feature.
//!
//! Wire format (canonical, see
//! `boards/tx-hal-riscv64-qemu-virt/src/debug_trace.rs`):
//!
//! ```text
//! txdbg:trap n=0x... kind=SY pc=0x... a7=0x... a0=0x... a1=0x... a2=0x...
//! txdbg:trap n=0x... kind=iPF|lPF|sPF|? pc=0x... stval=0x... ra=0x... a0=0x... a1=0x...
//! txdbg:ent  n=0x... pc=0x... a0=0x... sp=0x...
//! ```
//!
//! Each record is one line, prefixed `txdbg:` for grep, with
//! `key=0xHEX` pairs separated by single spaces. A `txdbg:trap n=K`
//! is paired with the *next* `txdbg:ent n=K+1` to show "syscall NR
//! returned a0=R" or "fault N was retried at the same pc".
//!
//! Output modes:
//! - `--summary` (default): paired timeline, one line per trap.
//! - `--syscalls`: only syscall records, with NR resolved to a
//!   stable mnemonic from the RV64 generic ABI.
//! - `--raw`: pass-through grep of `txdbg:` lines.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{optional_option_value, resolve_path};

pub(crate) fn trap_trace(root: &Path, args: Vec<String>) -> Result<()> {
    let serial = optional_option_value(&args, "--serial")
        .ok_or_else(|| "missing required option --serial PATH".to_string())?;
    let path = resolve_path(root, serial.into());
    let body = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let raw = args.iter().any(|a| a == "--raw");
    let syscalls_only = args.iter().any(|a| a == "--syscalls");

    if raw {
        for line in body.lines() {
            if line.starts_with("txdbg:") {
                println!("{line}");
            }
        }
        return Ok(());
    }

    let records = parse_records(&body)?;
    if records.is_empty() {
        return Err(format!(
            "no txdbg: records in {}\n\
             (was the kernel built with `--features trap-trace`?)",
            path.display()
        ));
    }

    let traps_total = records
        .iter()
        .filter(|r| matches!(r, Record::Trap(_)))
        .count();
    let entries_total = records
        .iter()
        .filter(|r| matches!(r, Record::Entry(_)))
        .count();
    println!(
        "trap-trace: {} traps, {} userspace re-entries (file: {})",
        traps_total,
        entries_total,
        path.display()
    );
    println!();

    let pairs = pair_traps_with_entries(&records);
    let syscall_names = rv64_generic_syscall_names();

    let mut printed = 0usize;
    for pair in &pairs {
        match pair.trap {
            TrapRecord::Syscall {
                n,
                pc,
                a7,
                a0,
                a1,
                a2,
            } => {
                let name = syscall_names.get(&a7).copied().unwrap_or("<unknown>");
                let ret = pair
                    .entry
                    .as_ref()
                    .map(|e| format_syscall_return(e.a0))
                    .unwrap_or_else(|| "<no entry>".into());
                println!(
                    "[{n:#06x}] SY pc={pc:#012x}  {name:<24} a7={a7:#x}  a0={a0:#x} a1={a1:#x} a2={a2:#x}  -> {ret}"
                );
                printed += 1;
            }
            TrapRecord::Fault {
                n,
                kind,
                pc,
                stval,
                ra,
            } => {
                if syscalls_only {
                    continue;
                }
                println!("[{n:#06x}] {kind:<3} pc={pc:#012x}  stval={stval:#x}  ra={ra:#x}");
                printed += 1;
            }
        }
    }
    if !syscalls_only {
        println!();
        println!("(use --syscalls to filter to syscall records only;");
        println!(" --raw to pass-through grep `txdbg:` lines)");
    }
    if printed == 0 {
        return Err("no records matched the requested filter".into());
    }

    Ok(())
}

#[derive(Debug)]
enum Record {
    Trap(TrapRecord),
    Entry(EntryRecord),
}

#[derive(Debug, Clone, Copy)]
enum TrapRecord {
    Syscall {
        n: u64,
        pc: u64,
        a7: u64,
        a0: u64,
        a1: u64,
        a2: u64,
    },
    Fault {
        n: u64,
        kind: &'static str,
        pc: u64,
        stval: u64,
        ra: u64,
    },
}

impl TrapRecord {
    fn n(&self) -> u64 {
        match self {
            Self::Syscall { n, .. } | Self::Fault { n, .. } => *n,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct EntryRecord {
    n: u64,
    a0: u64,
}

#[derive(Debug)]
struct TrapEntryPair {
    trap: TrapRecord,
    entry: Option<EntryRecord>,
}

fn parse_records(body: &str) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    for (lineno, line) in body.lines().enumerate() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("txdbg:trap ") {
            let kv = parse_kv(rest);
            let n = kv.get("n").copied().unwrap_or(0);
            let pc = kv.get("pc").copied().unwrap_or(0);
            let kind = match rest
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.strip_prefix("kind="))
            {
                Some(k) => k,
                None => continue,
            };
            match kind {
                "SY" => {
                    out.push(Record::Trap(TrapRecord::Syscall {
                        n,
                        pc,
                        a7: kv.get("a7").copied().unwrap_or(0),
                        a0: kv.get("a0").copied().unwrap_or(0),
                        a1: kv.get("a1").copied().unwrap_or(0),
                        a2: kv.get("a2").copied().unwrap_or(0),
                    }));
                }
                k @ ("iPF" | "lPF" | "sPF" | "?") => {
                    let static_kind = match k {
                        "iPF" => "iPF",
                        "lPF" => "lPF",
                        "sPF" => "sPF",
                        _ => "?",
                    };
                    out.push(Record::Trap(TrapRecord::Fault {
                        n,
                        kind: static_kind,
                        pc,
                        stval: kv.get("stval").copied().unwrap_or(0),
                        ra: kv.get("ra").copied().unwrap_or(0),
                    }));
                }
                other => {
                    return Err(format!(
                        "line {}: unknown txdbg:trap kind={}",
                        lineno + 1,
                        other
                    ));
                }
            }
        } else if let Some(rest) = line.strip_prefix("txdbg:ent ") {
            let kv = parse_kv(rest);
            let n = kv.get("n").copied().unwrap_or(0);
            let a0 = kv.get("a0").copied().unwrap_or(0);
            out.push(Record::Entry(EntryRecord { n, a0 }));
        }
    }
    Ok(out)
}

fn parse_kv(s: &str) -> HashMap<&str, u64> {
    let mut out = HashMap::new();
    for token in s.split_whitespace() {
        if let Some((k, v)) = token.split_once('=') {
            let v = v.trim_start_matches("0x");
            if let Ok(parsed) = u64::from_str_radix(v, 16) {
                out.insert(k, parsed);
            }
        }
    }
    out
}

/// Pair each trap record with the *next* `txdbg:ent` whose `n`
/// equals `trap.n + 1` (the kernel emits the entry record as the
/// trap counter's current value, after `record_trap` already
/// incremented it). If no entry record matches (e.g. the trap was
/// `exit_group` and never re-entered userspace), the entry slot is
/// `None`.
fn pair_traps_with_entries(records: &[Record]) -> Vec<TrapEntryPair> {
    let entries_by_n: HashMap<u64, EntryRecord> = records
        .iter()
        .filter_map(|r| match r {
            Record::Entry(e) => Some((e.n, *e)),
            _ => None,
        })
        .collect();
    records
        .iter()
        .filter_map(|r| match r {
            Record::Trap(t) => Some(TrapEntryPair {
                trap: *t,
                entry: entries_by_n.get(&(t.n() + 1)).copied(),
            }),
            _ => None,
        })
        .collect()
}

fn format_syscall_return(a0: u64) -> String {
    let signed = a0 as i64;
    if (-4095..0).contains(&signed) {
        format!("-{} (errno {})", -signed, errno_name(-signed as u32))
    } else {
        format!("{a0:#x} ({signed})")
    }
}

fn errno_name(errno: u32) -> &'static str {
    match errno {
        1 => "EPERM",
        2 => "ENOENT",
        4 => "EINTR",
        5 => "EIO",
        9 => "EBADF",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        17 => "EEXIST",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        25 => "ENOTTY",
        38 => "ENOSYS",
        _ => "?",
    }
}

/// RV64 generic ABI syscall NR → mnemonic. Source of truth: Linux
/// `asm-generic/unistd.h` and `crates/tx-shims/src/linux_syscall/numbers.rs`.
/// Kept in xtask intentionally — the parser must work even when the
/// kernel-side dispatch table changes (we want to see "this NR was
/// invoked but is unimplemented" decode correctly).
fn rv64_generic_syscall_names() -> HashMap<u64, &'static str> {
    let pairs: &[(u64, &'static str)] = &[
        (17, "getcwd"),
        (23, "dup"),
        (24, "dup3"),
        (25, "fcntl"),
        (29, "ioctl"),
        (33, "mknodat"),
        (34, "mkdirat"),
        (35, "unlinkat"),
        (36, "symlinkat"),
        (37, "linkat"),
        (38, "renameat"),
        (39, "umount2"),
        (40, "mount"),
        (43, "statfs"),
        (44, "fstatfs"),
        (45, "truncate"),
        (46, "ftruncate"),
        (48, "faccessat"),
        (49, "chdir"),
        (50, "fchdir"),
        (53, "fchmodat"),
        (55, "fchownat"),
        (56, "openat"),
        (57, "close"),
        (59, "pipe2"),
        (61, "getdents64"),
        (62, "lseek"),
        (63, "read"),
        (64, "write"),
        (65, "readv"),
        (66, "writev"),
        (67, "pread64"),
        (68, "pwrite64"),
        (72, "pselect6"),
        (73, "ppoll"),
        (78, "readlinkat"),
        (79, "newfstatat"),
        (80, "fstat"),
        (88, "utimensat"),
        (93, "exit"),
        (94, "exit_group"),
        (95, "waitid"),
        (96, "set_tid_address"),
        (98, "futex"),
        (99, "set_robust_list"),
        (100, "get_robust_list"),
        (101, "nanosleep"),
        (113, "clock_gettime"),
        (114, "clock_getres"),
        (115, "clock_nanosleep"),
        (116, "syslog"),
        (124, "sched_yield"),
        (129, "kill"),
        (130, "tkill"),
        (131, "tgkill"),
        (132, "sigaltstack"),
        (133, "rt_sigsuspend"),
        (134, "rt_sigaction"),
        (135, "rt_sigprocmask"),
        (136, "rt_sigpending"),
        (137, "rt_sigtimedwait"),
        (138, "rt_sigqueueinfo"),
        (139, "rt_sigreturn"),
        (140, "setpriority"),
        (141, "getpriority"),
        (153, "times"),
        (154, "setpgid"),
        (155, "getpgid"),
        (156, "getsid"),
        (157, "setsid"),
        (160, "uname"),
        (165, "getrusage"),
        (166, "umask"),
        (172, "getpid"),
        (173, "getppid"),
        (174, "getuid"),
        (175, "geteuid"),
        (176, "getgid"),
        (177, "getegid"),
        (178, "gettid"),
        (179, "sysinfo"),
        (210, "shutdown"),
        (213, "epoll_create1"),
        (214, "brk"),
        (215, "munmap"),
        (216, "mremap"),
        (220, "clone"),
        (221, "execve"),
        (222, "mmap"),
        (226, "mprotect"),
        (227, "msync"),
        (233, "madvise"),
        (242, "accept4"),
        (260, "wait4"),
        (261, "prlimit64"),
        (276, "renameat2"),
        (278, "getrandom"),
        (291, "statx"),
        (435, "clone3"),
        (439, "faccessat2"),
    ];
    pairs.iter().copied().collect()
}
