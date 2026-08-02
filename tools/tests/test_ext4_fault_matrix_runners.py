import importlib.util
import os
import subprocess
import tempfile
import unittest
from unittest import mock
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
LINUX_RUNNER = ROOT / "tools" / "ext4" / "fault_linux_rw_replay.py"
TX_RUNNER = ROOT / "tools" / "ext4" / "fault_tx_remount.py"
TX_SCRIPT = ROOT / "tools" / "shell-tests" / "ext4-fault-tx-remount.txt"


def load_module(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class Ext4FaultMatrixRunnerTests(unittest.TestCase):
    def test_repository_owned_runners_and_tx_script_exist(self):
        self.assertTrue(LINUX_RUNNER.is_file())
        self.assertTrue(TX_RUNNER.is_file())
        self.assertTrue(os.access(LINUX_RUNNER, os.X_OK))
        self.assertTrue(os.access(TX_RUNNER, os.X_OK))
        self.assertTrue(TX_SCRIPT.is_file())

    def test_linux_runner_fails_closed_off_linux(self):
        with tempfile.TemporaryDirectory() as tmp:
            image = Path(tmp) / "replay.img"
            image.write_bytes(b"not-an-ext4-image")
            module = load_module(LINUX_RUNNER, "fault_linux_rw_replay")
            with mock.patch.object(module.platform, "system", return_value="Darwin"):
                with self.assertRaisesRegex(module.LinuxReplayError, "requires a Linux host"):
                    module.validate_host(image)

    def test_linux_runner_can_preflight_with_docker_fallback_off_linux(self):
        module = load_module(LINUX_RUNNER, "fault_linux_rw_replay")
        completed = subprocess.CompletedProcess(["docker"], 0, "", "")
        with mock.patch.object(module.platform, "system", return_value="Darwin"), mock.patch.object(
            module.shutil, "which", return_value="/usr/bin/docker"
        ), mock.patch.object(module.subprocess, "run", side_effect=[completed, completed]) as run:
            self.assertEqual(module.select_replay_backend(), "docker")

        self.assertEqual(run.call_count, 2)
        self.assertIn("image", run.call_args_list[0].args[0])
        self.assertIn("run", run.call_args_list[1].args[0])

    def test_linux_runner_fails_preflight_when_host_and_docker_are_unavailable(self):
        module = load_module(LINUX_RUNNER, "fault_linux_rw_replay")
        with mock.patch.object(module.platform, "system", return_value="Darwin"), mock.patch.object(
            module.shutil, "which", return_value=None
        ):
            with self.assertRaisesRegex(module.LinuxReplayError, "Docker fallback blocked"):
                module.preflight_replay_environment()

    def test_tx_runner_builds_shell_test_with_matrix_image_as_scratch(self):
        with tempfile.TemporaryDirectory() as tmp:
            image = Path(tmp) / "tx-remount.img"
            image.write_bytes(b"ext4-image")
            module = load_module(TX_RUNNER, "fault_tx_remount")
            command = module.build_shell_test_command(image, ROOT, dict(os.environ))

        self.assertEqual(command[:3], ["cargo", "xtask", "shell-test"])
        self.assertIn("--extra-rv64-ext4", command)
        self.assertEqual(command[command.index("--extra-rv64-ext4") + 1], str(image))
        self.assertEqual(command[command.index("--script") + 1], str(TX_SCRIPT))
        self.assertEqual(command[command.index("--target") + 1], "rv64-qemu")
        self.assertEqual(command[command.index("--profile") + 1], "busybox")

    def test_tx_remount_script_uses_single_busybox_matrix_device(self):
        text = TX_SCRIPT.read_text(encoding="utf-8")
        self.assertIn("/dev/block/vda", text)
        self.assertNotIn("/dev/block/vdb", text)
        self.assertNotIn("fault-cut.txt", text)

    def test_tx_runner_rejects_missing_matrix_image_before_qemu(self):
        result = subprocess.run(
            ["python3", str(TX_RUNNER), str(ROOT / "target" / "missing-remount.img")],
            cwd=ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing replay matrix image", result.stderr)

    def test_tx_runner_accepts_executor_case_cut_image_shape(self):
        result = subprocess.run(
            [
                "python3",
                str(TX_RUNNER),
                "D7",
                "crash-cut-0007",
                str(ROOT / "target" / "missing-remount.img"),
            ],
            cwd=ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing replay matrix image", result.stderr)


if __name__ == "__main__":
    unittest.main()
