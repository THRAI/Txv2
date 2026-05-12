use std::path::Path;

use crate::util::{compact_cargo, tail_lines, CargoOutcome};
use crate::Result;

const HOST_PACKAGES: &[&str] = &["tx-shims", "tx-kernel", "tx-ext4", "tx-scripts"];

pub(crate) fn unit(root: &Path) -> Result<()> {
    let mut build_args: Vec<&str> = vec!["build"];
    for pkg in HOST_PACKAGES {
        build_args.push("-p");
        build_args.push(pkg);
    }

    let build_label = format!("build  {}", HOST_PACKAGES.join(" "));
    let build = compact_cargo(root, &build_args);
    print_step(&build_label, &build);
    if !build.ok {
        return Err("build failed".into());
    }

    let mut failed = false;
    for pkg in HOST_PACKAGES {
        let test_args = ["test", "-p", pkg, "--lib", "--", "--test-threads=1"];
        let label = format!("test   {pkg} (lib)");
        let result = compact_cargo(root, &test_args);
        let ok = result.ok;
        print_step(&label, &result);
        if !ok {
            failed = true;
        }
    }

    if failed {
        Err("unit tests failed".into())
    } else {
        Ok(())
    }
}

fn print_step(label: &str, result: &CargoOutcome) {
    if result.ok {
        println!("✓ {label}  {}", result.summary);
    } else {
        println!("✗ {label}  FAIL");
        let snippet = tail_lines(&result.output, 40);
        for line in snippet.lines() {
            let t = line.trim();
            if t.starts_with("test ") && t.ends_with("... ok") {
                continue;
            }
            if t.starts_with("Compiling ")
                || t.starts_with("Finished ")
                || t.starts_with("Running ")
                || t.starts_with("running ")
                || t.starts_with("error: test failed")
                || t.starts_with("test result:")
            {
                continue;
            }
            println!("  {line}");
        }
    }
}
