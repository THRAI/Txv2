use std::env;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, String>;

mod boundary_report;
mod check_build;
mod ci;
mod doctor;
mod ext4;
mod fault_decode;
mod full_build;
mod image;
mod kernel_user_layouts;
mod lint;
mod lint_invariants_api_language;
mod lint_invariants_checks;
mod lint_invariants_cred_check;
mod lint_invariants_drive;
mod lint_invariants_ext4;
mod lint_invariants_notification;
mod lint_invariants_observe;
mod lint_invariants_script;
mod lint_invariants_signal;
mod lint_invariants_step;
mod lint_invariants_step_interface;
mod lint_invariants_step_v3;
mod lint_invariants_subj;
mod lint_invariants_syscall;
mod lint_invariants_time_layering;
mod lint_invariants_time_wake;
mod lint_invariants_wait;
mod lint_invariants_witness;
mod lint_invariants_zone;
#[path = "lint_step_guard.rs"]
mod lint_step_guard;
mod observe;
mod observe_discipline;
mod observe_schema;
mod oscomp;
mod progress;
mod qemu;
mod shell_test;
mod submit;
mod syscall;
mod syscall_ref;
mod syscall_status;
mod target;
mod test;
mod trap_trace;
mod unit;
mod util;

pub fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        print_usage();
        return Ok(());
    };

    let root = workspace_root();
    match cmd.as_str() {
        "doctor" => doctor::doctor(&root),
        "full-build" => full_build::full_build(&root, args.collect()),
        "ci" => ci::ci(&root),
        "ci-slow" => ci::ci_slow(&root),
        "check" => check_build::check(&root),
        "build" => {
            let rest: Vec<String> = args.collect();
            let target = util::option_value(&rest, "--target")?;
            if rest.iter().any(|arg| arg == "--release") {
                check_build::build_release(&root, &target)
            } else {
                check_build::build(&root, &target)
            }
        }
        "qemu" => qemu::qemu(&root, args.collect()),
        "test" => test::test(&root, args.collect()),
        "fault-decode" => fault_decode::fault_decode(&root, args.collect()),
        "trap-trace" => trap_trace::trap_trace(&root, args.collect()),
        "shell-test" => shell_test::shell_test(&root, args.collect()),
        "image" => image::image(&root, args.collect()),
        "ext4" => ext4::ext4(&root, args.collect()),
        "kernel-user-layouts" => kernel_user_layouts::kernel_user_layouts(&root, args.collect()),
        "oscomp" => oscomp::oscomp(&root, args.collect()),
        "submit" => submit::submit(&root, args.collect()),
        "syscall" => syscall::syscall(&root, args.collect()),
        "progress" => progress::progress(&root, args.collect()),
        "lint" => lint::lint(&root, args.collect()),
        "boundary-report" => boundary_report::boundary_report(&root, args.collect()),
        "unit" => unit::unit(&root),
        "observe" => observe::observe(&root, args.collect()),
        "observe-schema" => observe_schema::observe_schema(&root, args.collect()),
        "observe-discipline" => observe_discipline::observe_discipline(&root),
        "syscall-status" => syscall_status::syscall_status(&root, args.collect()),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

fn print_usage() {
    println!(
        "txKernel xtask\n\n\
         Commands:\n\
           cargo xtask doctor\n\
           cargo xtask full-build [--target rv64-qemu|la64-qemu|rv64-m1dock-mock|all] [--skip-doctor] [--no-image]\n\
           cargo xtask ci\n\
           cargo xtask ci-slow\n\
           cargo xtask check\n\
           cargo xtask build --target rv64-qemu|rv64-m1dock-mock|la64-qemu|all\n\
           cargo xtask qemu --target rv64-qemu|rv64-m1dock-mock|la64-qemu --profile smoke|busybox|alpine [--boot-mode normal|alpine|contest|busybox|oscomp|ltp|test] [--dry-run] [--expect-sentinel] [--timeout-ms N] [--smp N] [--no-block] [--interactive] [--append-cmdline TEXT] [--extra-rv64-ext4 PATH ...]\n\
           cargo xtask test [smoke|busybox-boot] [--target rv64-qemu] [--timeout-ms N] [--dry-run] [--trap-trace]\n\
           cargo xtask fault-decode --target rv64-qemu [--elf PATH] [--serial PATH [--all] | --scause HEX --sepc HEX --stval HEX | --addr HEX]\n\
           cargo xtask trap-trace --serial PATH [--syscalls | --raw]\n\
           cargo xtask shell-test --target rv64-qemu --script PATH [--boot-mode normal|alpine|contest|busybox|oscomp|ltp|test] [--extra-rv64-ext4 PATH ...] [--group NAME[,NAME...]] [--list-groups] [--keep-going]\n\
           cargo xtask image cpio --profile busybox [--target rv64-qemu|la64-qemu]\n\
           cargo xtask image ext4 --profile busybox [--target rv64-qemu|la64-qemu] [--size 64M]\n\
           cargo xtask image m1dock-sd --profile busybox [--target rv64-m1dock-mock] [--size 64M]\n\
           cargo xtask ext4 tier1 [--run-id RUN_ID] [--dry-run] [--preflight-live] [--materialize-xfstests] [--preflight-report PATH] [--resume] [--start-cut crash-cut-NNNN] [--verify-receipt PATH]\n\
           cargo xtask kernel-user-layouts [--arch riscv64|loongarch64] [--dump]\n\
           cargo xtask oscomp doctor|prepare|submit|run|qemu\n\
           cargo xtask oscomp score [--target rv64-qemu|la64-qemu] [--input FILE] [--suite SUITE] [--data DIR] [--dry-run]\n\
           cargo xtask oscomp list-suites [--target rv64-qemu|la64-qemu] [--data DIR]\n\
           cargo xtask oscomp test --target rv64-qemu|la64-qemu [--suite SUITE] [--skip-build] [--data DIR] [--dry-run]\n\
           cargo xtask oscomp slim-sdcard [--suite SUITE]... [--ltp-cases CASE1,CASE2] [--source IMG] [-o IMG] [--size-mb N]\n\
           cargo xtask submit k210 [--out target/submit/k210]\n\
           cargo xtask syscall status|list|info|sync|pick — query/maintain the syscall map (SSoT: numbers.rs + mod.rs)\n\
           cargo xtask progress validate\n\
           cargo xtask progress list plans|handoffs|worktrees|all [--json]\n\
           cargo xtask progress new plan|handoff|worktree --id ID --title TITLE [...]\n\
           cargo xtask progress claim plan|worktree --id ID --owner NAME --scope PATH [--scope PATH]\n\
           cargo xtask progress close plan|handoff|worktree --id ID --status STATUS\n\
           cargo xtask lint arch|docs|unused|boundary|invariants [rule|all]|kernel-user-layouts|syscall-status\n\
             invariants rule includes api-language, boot-setup, observe-producer-boundary, time-layering, time-wake-retired, no-adhoc-drive, syscall-no-await, step, and related discipline checks\n\
           cargo xtask boundary-report [--top N] [--json]\n\
           cargo xtask observe-schema check [--schema schema/txobserve.toml]\n\
           cargo xtask syscall-status [<NAME>...] [--regen|--check|--list-missing]\n\
           cargo xtask unit\n"
    );
}

fn workspace_root() -> PathBuf {
    if let Ok(mut cwd) = env::current_dir() {
        loop {
            if cwd.join("Cargo.toml").is_file() && cwd.join("xtask/Cargo.toml").is_file() {
                return cwd;
            }
            if !cwd.pop() {
                break;
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under workspace root")
        .to_path_buf()
}
