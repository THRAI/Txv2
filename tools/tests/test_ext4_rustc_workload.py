import importlib.util
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "tools/ext4/run_rustc_kernel_workload.py"
PATCHER = ROOT / "tools/images/patch-riscv64-glibc-interpreter.py"
MATERIALIZER = ROOT / "tools/ext4/materialize_rustc_kernel_workload.py"


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runner = load(RUNNER, "rustc_workload_runner")
patcher = load(PATCHER, "riscv64_interpreter_patcher")


class RustcWorkloadTests(unittest.TestCase):
    def test_runner_uses_current_named_roles_and_fixed_geometry(self):
        command = runner.command(
            Path("/tmp/tx"), Path("/tmp/scenario"), Path("/tmp/serial"),
            [Path("/tmp/test.img"), Path("/tmp/scratch.img"), Path("/tmp/workload.img")],
        )
        self.assertIn("--memory-mib", command)
        self.assertEqual(command[command.index("--memory-mib") + 1], "4096")
        self.assertEqual(command[command.index("--ext4-test-image") + 1], "/tmp/test.img")
        self.assertEqual(command[command.index("--ext4-scratch-image") + 1], "/tmp/scratch.img")
        self.assertEqual(command[command.index("--ext4-workload-image") + 1], "/tmp/workload.img")
        script = runner.scenario("/mnt/ext4-test/run-rustc-kernel-build.sh", 123)
        self.assertIn("mount -t ext4 -o rw /dev/block/vda", script)
        self.assertIn("TX_EXT4_TEST_DEVICE=/dev/block/vda", script)
        self.assertIn("TX_EXT4_WORKLOAD_DEVICE=/dev/block/vdc", script)
        self.assertNotIn('echo TX_GUEST_RUST_BUILD status=', script)
        self.assertIn('marker=TX_GUEST_RUST_BUILD; echo', script)
        self.assertIn('$marker status=$status', script)

    def test_workload_installer_is_scoped_to_copied_toolchain(self):
        installer = (ROOT / "tools/images/install-riscv64-rustc-resolv-shim.sh").read_text()
        self.assertIn("WORKLOAD_TOOLCHAIN", installer)
        self.assertNotIn("rootfs=", installer)
        guest = (ROOT / "tools/ext4/guest/run-rustc-kernel-build.sh").read_text()
        self.assertTrue(guest.startswith("#!/bin/sh\n"))
        self.assertIn("set -eu\n", guest)
        self.assertNotIn("pipefail", guest)
        self.assertIn('if ! test -f "$TX_EXT4_TEST_MOUNT/source/Cargo.lock"', guest)
        self.assertIn('mount -t ext4 -o rw "$TX_EXT4_TEST_DEVICE"', guest)
        self.assertIn('if ! test -f "$TX_EXT4_WORKLOAD_MOUNT/toolchain/bin/rustc"', guest)
        self.assertIn('mount -t ext4 -o ro "$TX_EXT4_WORKLOAD_DEVICE"', guest)
        self.assertIn('if [ -r /proc/swaps ]', guest)
        self.assertIn('test -x "$TX_EXT4_TOOLCHAIN_ROOT/bin/rustc"', guest)
        self.assertIn('PATH="$TX_EXT4_TOOLCHAIN_ROOT/bin:/bin"', guest)
        self.assertIn('TX_EXT4_MUSL_LOADER:=/lib/ld-musl-riscv64.so.1', guest)
        self.assertIn('TX_EXT4_RUSTC_WRAPPER:=$TX_EXT4_TEST_MOUNT/rustc-via-musl-loader', guest)
        self.assertIn('export TX_EXT4_MUSL_LOADER TX_EXT4_TOOLCHAIN_ROOT', guest)
        self.assertIn('LD_PRELOAD="/lib/libgcompat.so.0:', guest)
        self.assertIn('RUSTC="$TX_EXT4_RUSTC_WRAPPER"', guest)
        self.assertIn('"$TX_EXT4_MUSL_LOADER" "$TX_EXT4_TOOLCHAIN_ROOT/bin/cargo"', guest)
        self.assertIn("TX_GUEST_RUST_STAGE=preflight", guest)
        self.assertIn("TX_GUEST_RUST_STAGE=rustc-version-ok", guest)
        self.assertIn("TX_GUEST_RUST_STAGE=cargo-build-ok", guest)
        self.assertIn("build --release -vv", guest)
        self.assertIn('--manifest-path "$TX_EXT4_TEST_MOUNT/source/Cargo.toml"', guest)
        self.assertNotIn('cd "$TX_EXT4_TEST_MOUNT/source"', guest)
        self.assertIn('test -f "$TX_EXT4_RUSTC_WRAPPER"', guest)
        materializer = MATERIALIZER.read_text()
        self.assertIn('wrapper = test / "rustc-via-musl-loader"', materializer)
        self.assertIn('os.chmod(wrapper, 0o755)', materializer)
        self.assertIn('/bin/busybox umount', guest)
        self.assertIn('mount -t ext4 -o ro "$TX_EXT4_WORKLOAD_DEVICE"', guest)
        self.assertIn("--offline --frozen", guest)

    def test_materializer_admits_only_the_tier1_ext4_profile(self):
        materializer = MATERIALIZER.read_text()
        self.assertIn("HEADROOM_BYTES = 512 * 1024 * 1024", materializer)
        self.assertIn('TIER1_MKFS_FEATURES = "^orphan_file"', materializer)
        self.assertIn("TIER1_JOURNAL_SIZE_MIB = 4", materializer)
        self.assertIn('"-O", TIER1_MKFS_FEATURES', materializer)
        self.assertIn('"-J", f"size={TIER1_JOURNAL_SIZE_MIB}"', materializer)
        self.assertIn("find_e2fsck", materializer)
        self.assertIn('[e2fsck, "-fy", str(image)]', materializer)

    def test_patcher_allows_idempotent_noop(self):
        with tempfile.TemporaryDirectory() as temporary:
            self.assertEqual(patcher.patch_tree(Path(temporary)), 0)


if __name__ == "__main__":
    unittest.main()
