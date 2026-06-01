import importlib.util
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "oscomp-observe-live.py"


def load_module():
    spec = importlib.util.spec_from_file_location("oscomp_observe_live", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class OscompObserveLiveTests(unittest.TestCase):
    def test_default_layout_seals_complex_artifact_paths(self):
        mod = load_module()
        with TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            layout = mod.build_layout(root, "pthread")

            self.assertEqual(layout.base, root / "target/oscomp/custom-run/pthread")
            self.assertEqual(layout.host_dir, layout.base / "host")
            self.assertEqual(layout.guest_mem, layout.host_dir / "guest-ram.bin")
            self.assertEqual(layout.stop_file, layout.host_dir / "stop")
            self.assertEqual(layout.serial, layout.base / "serial.txt")
            self.assertEqual(layout.data, layout.base / "data")
            self.assertEqual(layout.submit, layout.base / "submit")
            self.assertEqual(layout.names, layout.base / "names.json")
            self.assertEqual(layout.parquet_dir, layout.base / "analysis/parquet")
            self.assertEqual(layout.cache_dir, layout.base / "analysis/cache")
            self.assertEqual(layout.report, layout.base / "report.json")

    def test_plan_uses_raw_only_live_drain_and_parquet_analysis(self):
        mod = load_module()
        with TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            layout = mod.build_layout(root, "pthread")
            args = mod.WorkflowArgs(
                name="pthread",
                output_dir=None,
                python_file=Path("analysis.py"),
                only="pthread",
                timeout=120,
                smp=4,
                ram_size="1G",
                ring_bytes=2097152,
                poll_ms=1,
                source_data=Path("target/oscomp/testdata-full"),
                skip_build=True,
                skip_submit=True,
                keep_guest_mem=False,
                dry_run=True,
            )

            plan = mod.build_plan(root, args, layout)
            commands = [" ".join(str(part) for part in command.argv) for command in plan.commands]

            self.assertIn("--observe-bracket", commands[0])
            self.assertIn("--libcbench-only pthread", commands[0])
            self.assertIn("memory-backend-file,id=txram,size=1G", commands[3])
            self.assertIn("mem-path=" + str(layout.guest_mem), commands[3])
            self.assertIn("tx.oscomp.observe=0", commands[3])
            self.assertIn("tx.oscomp.observe_live_drain=1", commands[3])
            self.assertIn("tx.oscomp.groups=libcbench-musl", commands[3])
            self.assertIn("live-guest-mem", commands[4])
            self.assertIn("--guest-mem " + str(layout.guest_mem), commands[4])
            self.assertIn("--output-dir " + str(layout.host_dir), commands[4])
            self.assertNotIn("--finalize", commands[4])
            self.assertIn("--rawrecords " + str(layout.rawrecords), commands[5])
            self.assertIn("--parquet-dir " + str(layout.parquet_dir), commands[5])
            self.assertIn("--python-file analysis.py", commands[5])

    def test_public_test_selector_maps_to_libcbench_selection(self):
        mod = load_module()

        self.assertEqual(mod.select_libcbench_test("pthread"), "pthread")
        self.assertEqual(mod.select_libcbench_test("vm"), "malloc-vm")
        self.assertEqual(mod.select_libcbench_test("malloc-big2"), "malloc-big2")

        with self.assertRaisesRegex(ValueError, "unknown --test"):
            mod.select_libcbench_test("not-a-test")

    def test_parse_args_prefers_public_test_selector_over_default(self):
        mod = load_module()

        args = mod.parse_args(["--test", "stdio", "--dry-run"])

        self.assertEqual(args.only, "stdio")

    def test_plan_refreshes_kernel_submit_when_not_skipped(self):
        mod = load_module()
        with TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            layout = mod.build_layout(root, "pthread")
            args = mod.WorkflowArgs(
                name="pthread",
                output_dir=None,
                python_file=None,
                only="pthread",
                timeout=120,
                smp=4,
                ram_size="1G",
                ring_bytes=2097152,
                poll_ms=1,
                source_data=Path("target/oscomp/testdata-full"),
                skip_build=False,
                skip_submit=False,
                keep_guest_mem=False,
                dry_run=True,
            )

            labels = [command.label for command in mod.build_plan(root, args, layout).commands]

            self.assertEqual(
                labels[:5],
                [
                    "prepare-image",
                    "build-kernel",
                    "submit-kernel",
                    "names",
                    "truncate-guest-mem",
                ],
            )

    def test_output_dir_overrides_default_layout_base(self):
        mod = load_module()
        with TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            out = root / "custom/out"
            layout = mod.build_layout(root, "ignored", output_dir=out)

            self.assertEqual(layout.base, out)
            self.assertEqual(layout.host_dir, out / "host")


if __name__ == "__main__":
    unittest.main()
