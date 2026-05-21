import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-kernel-user-layouts.py"


def load_checker():
    spec = importlib.util.spec_from_file_location("kernel_user_layout_checker", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class KernelUserLayoutCheckerTests(unittest.TestCase):
    def test_registered_rust_backed_structs_must_have_marker_impl(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            syscall_dir = root / "crates" / "tx-shims" / "src" / "linux_syscall"
            syscall_dir.mkdir(parents=True)
            (syscall_dir / "demo.rs").write_text(
                "#[repr(C)]\n"
                "struct MissingLayout {\n"
                "    value: u64,\n"
                "}\n"
            )
            rust = {
                "candidates": [
                    {
                        "name": "struct missing",
                        "rust_type": "MissingLayout",
                        "status": "checked",
                    }
                ]
            }

            self.assertEqual(
                checker.check_source_marker_coverage(root, rust),
                [
                    "crates/tx-shims/src/linux_syscall/demo.rs:2: MissingLayout: registered #[repr(C)] kernel/user ABI rust_type must implement KernelToUserLayout"
                ],
            )

    def test_unregistered_repr_c_structs_do_not_require_marker_impl(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            syscall_dir = root / "crates" / "tx-shims" / "src" / "linux_syscall"
            syscall_dir.mkdir(parents=True)
            (syscall_dir / "raw_uapi.rs").write_text(
                "#[repr(C)]\n"
                "struct RawUapiLayout {\n"
                "    value: u64,\n"
                "}\n"
            )
            rust = {
                "candidates": [
                    {
                        "name": "struct registered",
                        "rust_type": "",
                        "status": "manual",
                    }
                ]
            }

            self.assertEqual(checker.check_source_marker_coverage(root, rust), [])

    def test_repr_c_kernel_user_struct_with_marker_impl_passes_source_lint(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            syscall_dir = root / "crates" / "tx-shims" / "src" / "linux_syscall"
            syscall_dir.mkdir(parents=True)
            (syscall_dir / "demo.rs").write_text(
                "#[repr(C)]\n"
                "struct DemoLayout {\n"
                "    value: u64,\n"
                "}\n\n"
                "impl KernelToUserLayout for DemoLayout {\n"
                "    const LAYOUT: KernelUserLayout = todo!();\n"
                "}\n"
            )
            rust = {
                "candidates": [
                    {
                        "name": "struct demo",
                        "rust_type": "DemoLayout",
                        "status": "checked",
                    }
                ]
            }

            self.assertEqual(checker.check_source_marker_coverage(root, rust), [])

    def test_repr_c_kernel_user_struct_with_marker_macro_passes_source_lint(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            syscall_dir = root / "crates" / "tx-shims" / "src" / "linux_syscall"
            syscall_dir.mkdir(parents=True)
            (syscall_dir / "demo.rs").write_text(
                "#[repr(C)]\n"
                "struct DemoLayout {\n"
                "    value: u64,\n"
                "}\n\n"
                "marked_kernel_user_layout!(\n"
                "    DemoLayout,\n"
                "    header: \"demo.h\",\n"
                "    musl: \"struct demo\",\n"
                "    fields: [(value, \"value\")],\n"
                ");\n"
            )
            rust = {
                "candidates": [
                    {
                        "name": "struct demo",
                        "rust_type": "DemoLayout",
                        "status": "checked",
                    }
                ]
            }

            self.assertEqual(checker.check_source_marker_coverage(root, rust), [])

    def test_duplicate_registered_kernel_user_struct_names_are_a_redlight(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            syscall_dir = root / "crates" / "tx-shims" / "src" / "linux_syscall"
            syscall_dir.mkdir(parents=True)
            (syscall_dir / "a.rs").write_text(
                "#[repr(C)]\n"
                "struct DupLayout {\n"
                "    value: u64,\n"
                "}\n"
            )
            (syscall_dir / "b.rs").write_text(
                "#[repr(C)]\n"
                "struct DupLayout {\n"
                "    value: u64,\n"
                "}\n"
            )
            rust = {
                "candidates": [
                    {
                        "name": "struct dup",
                        "rust_type": "DupLayout",
                        "status": "checked",
                    }
                ]
            }

            self.assertEqual(
                checker.check_source_marker_coverage(root, rust),
                [
                    "DupLayout: duplicate registered #[repr(C)] kernel/user ABI rust_type at crates/tx-shims/src/linux_syscall/a.rs:2, crates/tx-shims/src/linux_syscall/b.rs:2; use a shared marked type or a unique name"
                ],
            )

    def test_compare_arch_reports_size_align_and_offset_mismatch(self):
        checker = load_checker()
        rust = {
            "candidates": [
                {
                    "name": "DemoLayout",
                    "status": "checked",
                    "musl_header": "demo.h",
                    "musl_type": "struct demo",
                    "size": 16,
                    "align": 8,
                    "fields": [
                        {"rust": "first", "musl": "first", "offset": 0},
                        {"rust": "second", "musl": "second", "offset": 8},
                    ],
                }
            ]
        }
        musl = {
            0: {
                "rust_type": "DemoLayout",
                "size": 24,
                "align": 4,
                "fields": {
                    "first": 0,
                    "second": 12,
                },
            }
        }

        self.assertEqual(
            checker.compare_arch("riscv64", rust, musl),
            [
                "riscv64:DemoLayout: size rust=16 musl=24",
                "riscv64:DemoLayout: align rust=8 musl=4",
                "riscv64:DemoLayout.second: offset rust=8 musl=12 (musl field second)",
            ],
        )

    def test_missing_candidate_coverage_is_a_redlight(self):
        checker = load_checker()
        rust = {
            "layouts": [],
            "candidates": [
                {
                    "name": "struct epoll_event",
                    "kind": "manual",
                    "musl_header": "sys/epoll.h",
                    "musl_type": "struct epoll_event",
                    "status": "checked",
                    "size": 16,
                    "align": 8,
                    "fields": [],
                },
                {
                    "name": "struct pollfd",
                    "kind": "manual",
                    "musl_header": "poll.h",
                    "musl_type": "struct pollfd",
                    "status": "unchecked",
                    "size": 8,
                    "align": 4,
                    "fields": [],
                },
            ],
        }

        self.assertEqual(
            checker.check_candidate_coverage(rust),
            [
                "struct pollfd: candidate status must be checked, prefix, manual, deferred, or excluded (got unchecked)"
            ],
        )


if __name__ == "__main__":
    unittest.main()
