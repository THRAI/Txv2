//! Immediate (Lane 1) syscall dispatch helpers.
//!
//! These are pure ABI queries that never yield, never enter StepOp,
//! never call drive()/drive_oneshot(), and never access VFS/VM/reactor/timer.
//!
//! ## Current layout (2026-05-15)
//!
//! The implementations are still in their original module files
//! (proc.rs, cred.rs, time.rs, signal.rs, vm.rs, fs_path.rs, fs_basic.rs).
//! This file exists as a documentation anchor and will absorb the
//! implementations when we extract them from those modules.
//!
//! ## Immediate syscalls (23)
//!
//! | Syscall            | Module    | Function             |
//! |--------------------|-----------|----------------------|
//! | getpid             | proc.rs   | sys_getpid           |
//! | getppid            | proc.rs   | sys_getppid          |
//! | getpgid            | proc.rs   | sys_getpgid          |
//! | getpgrp            | proc.rs   | sys_getpgrp          |
//! | getsid             | proc.rs   | sys_getsid           |
//! | getuid             | cred.rs   | sys_getuid           |
//! | geteuid            | cred.rs   | sys_geteuid          |
//! | getgid             | cred.rs   | sys_getgid           |
//! | getegid            | cred.rs   | sys_getegid          |
//! | getresuid          | cred.rs   | sys_getresuid        |
//! | getresgid          | cred.rs   | sys_getresgid        |
//! | times              | time.rs   | sys_times            |
//! | gettimeofday       | time.rs   | sys_gettimeofday     |
//! | umask              | fs_path.rs| sys_umask            |
//! | uname              | fs_basic.rs| sys_uname           |
//! | prlimit64          | misc.rs   | sys_prlimit64        |
//! | rt_sigreturn       | signal.rs | sys_rt_sigreturn     |
//! | set_tid_address    | proc.rs   | sys_set_tid_address  |
//! | set_robust_list    | proc.rs   | sys_set_robust_list  |
//! | madvise            | vm.rs     | sys_madvise          |
//! | mlock              | vm.rs     | sys_mlock            |
//! | munlock            | vm.rs     | sys_munlock          |
//! | utimensat          | fs_mut.rs | sys_utimensat        |
//!
//! When extracting, move each function body here, keep `pub(super) fn sys_*`
//! visibility, and add `use super::*;` for shared types (SyscallCtx,
//! SyscallResult, errno values, etc.).

// Placeholder: re-export nothing yet. The dispatch in mod.rs calls
// the original functions directly (sys_getpid, sys_getuid, etc.).
