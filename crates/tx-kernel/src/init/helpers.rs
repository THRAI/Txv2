//! Bootstrap helpers extracted from `init.rs` to keep the file under
//! the arch-lint line-count ceiling (1800 lines).

use tx_hal::TxPlatform;

/// Synchronous poll loop for bootstrap futures. Mirrors
/// `tx_scripts::drive::block_on` but does not depend on the reactor
/// or the process table — used before the reactor is fully up.
pub(super) fn bootstrap_block_on<F: core::future::Future>(mut future: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // Build a no-op waker without `alloc::sync::Arc`'s `Wake` trait,
    // which isn't available in the bootstrap phase (no `alloc` /
    // `Arc::new` plumbing wired by the time `bootstrap_block_on` is
    // first called).
    unsafe fn clone_raw(_data: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake_raw(_data: *const ()) {}
    unsafe fn wake_by_ref_raw(_data: *const ()) {}
    unsafe fn drop_raw(_data: *const ()) {}
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);

    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: the no-op vtable never dereferences `data`. We pass a
    // null pointer; the waker is only used for polling, never for
    // actual wake-up (bootstrap futures are synchronous or time out).
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);

    // The future is expected to be synchronous — if it returns
    // Pending 1024 times in a row we've hit a bug (e.g. forgotten
    // `drive_oneshot` call). A panic here is better than an
    // infinite silent hang during boot.
    let mut pinned = unsafe { Pin::new_unchecked(&mut future) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {}
        }
    }
    panic!("bootstrap_block_on: future did not resolve in 1024 polls");
}

/// Human-readable short tag for each `ExecError` variant, used in
/// the boot log to annotate `init=/custom_binary` failures (e.g.
/// `exec init=binary:fail` post-mortem).
pub(super) fn exec_error_tag(error: &tx_scripts::process::exec::ExecError) -> &'static str {
    use tx_scripts::process::exec::ExecError as E;
    match error {
        E::PathTooLong => "path-too-long",
        E::PathNotFound => "path-not-found",
        E::NotADirectory => "not-a-directory",
        E::PermissionDenied => "permission-denied",
        E::SymlinkLoop => "symlink-loop",
        E::NotExecutable => "not-executable",
        E::InvalidArgument => "invalid-argument",
        E::OutOfMemory => "out-of-memory",
        E::Busy => "busy",
        E::IoError => "io-error",
        // Forward-compat: ExecError may grow new variants. Avoid a
        // build break if a future variant lands without a label here.
        #[allow(unreachable_patterns)]
        _ => "other",
    }
}

/// Parse the firmware command line for an `init=` token and the
/// `tx.profile=busybox` profile flag.
///
/// Resolution order (matches the Linux kernel's classic ordering):
///   1. If the cmdline contains `init=PATH`, use `PATH` (argv0 set
///      to `PATH`'s basename).
///   2. Otherwise, if the cmdline contains the standalone token
///      `tx.profile=busybox`, default to `/bin/sh` argv0=`sh`.
///   3. Otherwise, fall back to the bake-in `/init` fixture.
///
/// The cmdline is borrowed from `<P as BootInfoIf>::boot_info()`,
/// which the firmware (or QEMU `-append`) populates with a
/// `&'static str`; the returned byte slices share that lifetime.
pub(super) fn parse_init_from_cmdline<P: TxPlatform>() -> (&'static [u8], &'static [u8]) {
    let cmdline = match <P as tx_hal::BootInfoIf>::boot_info().cmdline {
        Some(s) => s,
        None => return (b"/init", b"init"),
    };
    for token in cmdline.split_ascii_whitespace() {
        if let Some(path) = token.strip_prefix("init=") {
            let argv0 = match path.rfind('/') {
                Some(idx) => &path[idx + 1..],
                None => path,
            };
            return (path.as_bytes(), argv0.as_bytes());
        }
    }
    if cmdline
        .split_ascii_whitespace()
        .any(|t| t == "tx.profile=busybox")
    {
        // Direct path to the busybox binary. /bin/sh is a symlink
        // pointing at "busybox" (relative); the walker follows
        // symlinks but we keep the canonical path for clearer
        // error reporting on bootstrap-exec failure.
        return (b"/bin/busybox", b"sh");
    }
    (b"/init", b"init")
}
