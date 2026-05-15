use std::env;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, String>;

mod boundary_report;
mod check_build;
mod ci;
mod doctor;
mod fault_decode;
mod full_build;
mod image;
mod lint;
mod lint_invariants_checks;
mod lint_invariants_drive;
mod lint_invariants_script;
mod lint_invariants_signal;
mod lint_invariants_step;
mod lint_invariants_step_v3;
mod lint_invariants_subj;
mod lint_invariants_syscall;
mod lint_invariants_witness;
mod observe;
mod observe_discipline;
mod oscomp;
mod progress;
mod qemu;
mod shell_test;
mod submit;
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
            check_build::build(&root, &target)
        }
        "qemu" => qemu::qemu(&root, args.collect()),
        "test" => test::test(&root, args.collect()),
        "fault-decode" => fault_decode::fault_decode(&root, args.collect()),
        "trap-trace" => trap_trace::trap_trace(&root, args.collect()),
        "shell-test" => shell_test::shell_test(&root, args.collect()),
        "image" => image::image(&root, args.collect()),
        "oscomp" => oscomp::oscomp(&root, args.collect()),
        "submit" => submit::submit(&root, args.collect()),
        "progress" => progress::progress(&root, args.collect()),
        "lint" => lint::lint(&root, args.collect()),
        "boundary-report" => boundary_report::boundary_report(&root, args.collect()),
        "unit" => unit::unit(&root),
        "observe" => observe::observe(&root, args.collect()),
        "observe-discipline" => observe_discipline::observe_discipline(&root),
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
           cargo xtask qemu --target rv64-qemu|rv64-m1dock-mock|la64-qemu --profile smoke|busybox [--dry-run] [--expect-sentinel] [--timeout-ms N] [--no-block] [--interactive]\n\
           cargo xtask test [smoke|busybox-boot] [--target rv64-qemu] [--timeout-ms N] [--dry-run] [--trap-trace]\n\
           cargo xtask fault-decode --target rv64-qemu [--elf PATH] [--serial PATH [--all] | --scause HEX --sepc HEX --stval HEX | --addr HEX]\n\
           cargo xtask trap-trace --serial PATH [--syscalls | --raw]\n\
           cargo xtask shell-test --target rv64-qemu --script PATH [--group NAME[,NAME...]] [--list-groups] [--keep-going]\n\
           cargo xtask image cpio --profile busybox [--target rv64-qemu|la64-qemu]\n\
           cargo xtask image ext4 --profile busybox [--target rv64-qemu|la64-qemu] [--size 64M]\n\
           cargo xtask image m1dock-sd --profile busybox [--target rv64-m1dock-mock] [--size 64M]\n\
           cargo xtask oscomp doctor|prepare|submit|run|qemu\n\
           cargo xtask submit k210 [--out target/submit/k210]\n\
           cargo xtask progress validate\n\
           cargo xtask progress list plans|handoffs|worktrees|all [--json]\n\
           cargo xtask progress new plan|handoff|worktree --id ID --title TITLE [...]\n\
           cargo xtask progress claim plan|worktree --id ID --owner NAME --scope PATH [--scope PATH]\n\
           cargo xtask progress close plan|handoff|worktree --id ID --status STATUS\n\
           cargo xtask lint arch|docs|unused|boundary|invariants [rule|all]\n\
           cargo xtask boundary-report [--top N] [--json]\n\
           cargo xtask unit\n"
    );
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under workspace root")
        .to_path_buf()
}
