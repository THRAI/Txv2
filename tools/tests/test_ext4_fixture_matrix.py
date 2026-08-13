import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "ext4" / "fixture_matrix.py"


def load_module():
    spec = importlib.util.spec_from_file_location("ext4_fixture_matrix", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Ext4FixtureMatrixTests(unittest.TestCase):
    def test_find_tool_checks_path_and_homebrew_prefix(self):
        mod = load_module()
        brew_tool = Path("/opt/homebrew/opt/e2fsprogs/sbin/debugfs")
        with mock.patch.object(mod.shutil, "which", return_value=None), mock.patch.object(mod.Path, "is_file", autospec=True, side_effect=lambda path: path == brew_tool):
            self.assertEqual(mod.find_tool("debugfs"), brew_tool)

    def test_require_tools_reports_every_missing_program(self):
        mod = load_module()
        with mock.patch.object(mod, "find_tool", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "missing required e2fsprogs tools:.*debugfs.*e2fsck.*mke2fs"):
                mod.require_tools()

    def test_manifest_is_stable_and_records_image_hash_and_tool_versions(self):
        mod = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            image = root / "clean-linear.ext4"
            image.write_bytes(b"deterministic image bytes")
            tools = {name: Path(f"/tools/{name}") for name in mod.REQUIRED_TOOLS}
            versions = {name: f"{name} 1.47.2" for name in mod.REQUIRED_TOOLS}
            first = mod.fixture_manifest(mod.FIXTURES["clean-linear"], image, tools, versions)
            second = mod.fixture_manifest(mod.FIXTURES["clean-linear"], image, tools, versions)
            self.assertEqual(first, second)
            self.assertEqual(first["schema"], "tx.ext4.fixture.v1")
            self.assertEqual(first["image"]["bytes"], len(image.read_bytes()))
            self.assertEqual(first["image"]["sha256"], mod.file_sha256(image))
            self.assertEqual(first["tools"]["debugfs"]["version"], "debugfs 1.47.2")
            json.dumps(first, sort_keys=True)

    def test_fixture_catalog_names_every_p0_shape(self):
        mod = load_module()
        self.assertEqual(set(mod.FIXTURES), {"clean-linear", "metadata-csum-64bit", "htree-large-dir", "sparse-large-file", "fragmented-depth2-extents", "links-and-symlinks", "dirty-journal", "unlinked-open-orphan"})
        for fixture in mod.FIXTURES.values():
            self.assertTrue(fixture.oracle_paths)
            self.assertTrue(fixture.capabilities)
        self.assertIn("/indexed", mod.FIXTURES["htree-large-dir"].oracle_paths)

    def test_clean_fixture_regeneration_is_byte_for_byte_reproducible(self):
        mod = load_module()
        try:
            mod.require_tools()
        except RuntimeError as error:
            self.skipTest(str(error))
        with tempfile.TemporaryDirectory() as first_tmp, tempfile.TemporaryDirectory() as second_tmp:
            mod.generate_host_fixture(mod.FIXTURES["clean-linear"], Path(first_tmp))
            mod.generate_host_fixture(mod.FIXTURES["clean-linear"], Path(second_tmp))
            first = Path(first_tmp) / "clean-linear.ext4"
            second = Path(second_tmp) / "clean-linear.ext4"
            self.assertEqual(mod.file_sha256(first), mod.file_sha256(second))
            self.assertEqual(first.read_bytes(), second.read_bytes())


if __name__ == "__main__":
    unittest.main()
