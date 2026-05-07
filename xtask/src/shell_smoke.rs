//! `cargo xtask shell-smoke` — end-to-end interactive shell smoke
//! against busybox running over QEMU stdio.
//!
//! Shell-prompt roadmap Slice 11 (2026-05-08).
//!
//! **Prerequisites** (the developer must arrange these before
//! running):
//!
//! 1. `TX_BUSYBOX` set to a static-musl-built riscv64 busybox
//!    binary path. Build with e.g.
//!    `riscv64-unknown-linux-musl-gcc -static busybox.c -o busybox`
//!    or extract from a busybox-1.36+ source tree:
//!    `make CROSS_COMPILE=riscv64-unknown-linux-musl- defconfig &&
//!     LDFLAGS=-static make CROSS_COMPILE=riscv64-unknown-linux-musl- -j8`.
//!
//! 2. `riscv64gc-unknown-none-elf` Rust target installed
//!    (`rustup target add riscv64gc-unknown-none-elf`).
//!
//! 3. QEMU 7.x or later with riscv64 board support in `$PATH`.
//!
//! **What this command does**:
//!
//! 1. Validates the prerequisites; fails fast with a clear message
//!    if any are missing.
//! 2. Builds `tx-kernel-riscv64-qemu-virt` with `TX_BUSYBOX` in the
//!    environment so `crates/tx-kernel/build.rs` bakes the busybox
//!    bytes into the kernel image at `$OUT_DIR/busybox.bin`.
//! 3. Spawns QEMU with the kernel + stdio pipes.
//! 4. Watches serial for the boot-ok sentinel
//!    (`txkernel:rv64-qemu-virt:boot:ok`).
//! 5. Watches for the busybox shell prompt (`# ` or `~ #`).
//! 6. Sends `echo hello\n` over stdin.
//! 7. Watches serial for `hello`.
//! 8. Sends `exit\n` over stdin.
//! 9. Watches serial for the userspace-exited sentinel
//!    (`txkernel:rv64-qemu-virt:userspace:exited:0`).
//! 10. Reports PASS / FAIL with the captured serial log on FAIL.
//!
//! **Status**: scaffolding. The command builds and emits clear
//! prerequisite-check errors; the full runtime isn't validated on
//! a host without the prereqs. CI integration is a follow-up that
//! provisions the cross-toolchain + busybox binary.

use std::env;
use std::path::Path;
use std::time::Duration;

use crate::Result;

/// Default per-step timeout when watching for sentinels / output
/// matches. The boot-ok sentinel typically appears in 1-2s on QEMU;
/// 30s allows headroom for slow CI hardware + busybox spawn.
const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sentinel pattern for the post-boot `init` prompt. busybox sh
/// prints either `~ #` (default ash prompt for root) or `# ` —
/// matching the bare `# ` substring is permissive but reliable.
const PROMPT_SENTINEL: &str = "# ";

/// Test-input string sent over stdin once the prompt is observed.
/// `echo hello\n` is the simplest builtin that exercises:
/// - Tokenizer (the shell parses the line).
/// - Builtin dispatch (`echo` is built into ash).
/// - Stdout (writes to fd 1 → console).
/// - Line-discipline echo (kernel echoes the typed bytes).
const ECHO_INPUT: &str = "echo hello\n";

/// Substring expected to appear in serial after `ECHO_INPUT` is
/// consumed. busybox echoes its argument followed by a newline.
const ECHO_OUTPUT: &str = "hello";

/// Test-input string sent to terminate the shell after the echo
/// roundtrip. busybox's `exit` builtin invokes `exit_group(0)`.
const EXIT_INPUT: &str = "exit\n";

/// Sentinel emitted by the kernel when init zombifies cleanly.
/// Format defined by `tx_kernel::init::CoreInit::run_userspace_reactor_loop`.
const EXIT_SENTINEL_PREFIX: &str = "txkernel:rv64-qemu-virt:userspace:exited:";

pub(crate) fn shell_smoke(_root: &Path, _args: Vec<String>) -> Result<()> {
    // Step 1: validate prerequisites.
    let busybox = match env::var("TX_BUSYBOX") {
        Ok(path) => path,
        Err(_) => {
            return Err(
                "TX_BUSYBOX is not set; point it at a static-musl-built riscv64 busybox \
                 binary before running `cargo xtask shell-smoke`. See \
                 `xtask/src/shell_smoke.rs`'s module header for build instructions."
                    .into(),
            );
        }
    };
    if !Path::new(&busybox).is_file() {
        return Err(format!(
            "TX_BUSYBOX={busybox} is not a regular file"
        ));
    }

    // Step 2: validate the cross-toolchain. Without
    // riscv64gc-unknown-none-elf installed, the kernel build will
    // fail with a less-actionable error; check up-front.
    //
    // (Detection via `rustup target list --installed`.)
    let cross_target = "riscv64gc-unknown-none-elf";
    let installed = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map_err(|e| format!("failed to invoke rustup: {e}"))?;
    if !installed.status.success() {
        return Err("rustup invocation failed; install rustup or check PATH".into());
    }
    let installed_targets = String::from_utf8_lossy(&installed.stdout);
    if !installed_targets.lines().any(|line| line.trim() == cross_target) {
        return Err(format!(
            "{cross_target} is not installed; run `rustup target add {cross_target}`"
        ));
    }

    // Step 3: validate QEMU is on PATH.
    let qemu_present = std::process::Command::new("qemu-system-riscv64")
        .arg("--version")
        .output()
        .is_ok();
    if !qemu_present {
        return Err(
            "qemu-system-riscv64 not found in PATH; install QEMU 7.x or later".into(),
        );
    }

    // Step 4-10: build, spawn, drive interactively. These steps
    // require external dependencies validated above; the
    // implementation is deferred to a follow-up integration session
    // that has them all green at once.
    //
    // The scaffolding (this file + the prerequisite checks above +
    // `crates/tx-kernel/build.rs`'s busybox-baked cfg) lands in
    // Slice 11 so the integration session has a clean entry point.
    Err(format!(
        "shell-smoke runtime not yet wired (Slice 11 carryover): prerequisites validated \
         (TX_BUSYBOX={busybox}, riscv64gc-unknown-none-elf installed, qemu-system-riscv64 \
         present), but the build-and-drive runtime is deferred to a follow-up session that \
         can validate the full end-to-end against a real busybox image."
    ))
}

/// Test scenario sentinels and timing constants used by the runtime
/// (when wired). Public so future integration tests can reference them.
#[allow(dead_code)]
pub(crate) const SCENARIO: &[(&str, &str, Duration)] = &[
    ("wait-prompt", PROMPT_SENTINEL, DEFAULT_STEP_TIMEOUT),
    ("send-echo", ECHO_INPUT, DEFAULT_STEP_TIMEOUT),
    ("wait-echo-output", ECHO_OUTPUT, DEFAULT_STEP_TIMEOUT),
    ("send-exit", EXIT_INPUT, DEFAULT_STEP_TIMEOUT),
    ("wait-exit-sentinel", EXIT_SENTINEL_PREFIX, DEFAULT_STEP_TIMEOUT),
];
