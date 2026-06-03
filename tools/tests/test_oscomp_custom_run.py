import importlib.util
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "oscomp-custom-run.py"


def load_module():
    spec = importlib.util.spec_from_file_location("oscomp_custom_run", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class OscompCustomRunTests(unittest.TestCase):
    def test_custom_script_uses_oscomp_group_markers_and_executes_binary(self):
        mod = load_module()

        script = mod.render_custom_testcode("probe", "hello-rv")

        self.assertIn("#### OS COMP TEST GROUP START probe ####", script)
        self.assertIn("./hello-rv", script)
        self.assertIn("custom-run:status:$?", script)
        self.assertIn("#### OS COMP TEST GROUP END probe ####", script)

    def test_libcbench_patch_inserts_observe_before_pthread_section(self):
        mod = load_module()
        source = """#include <unistd.h>
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_pthread_createjoin_serial1, 0);
}
"""

        patched = mod.patch_libcbench_main(source, threshold=30000, phase="pthread")

        self.assertIn("#include <sys/syscall.h>", patched)
        self.assertIn("extern long syscall(long, ...);", patched)
        self.assertIn("syscall(333, 30000);", patched)
        self.assertLess(
            patched.index("syscall(333, 30000);"),
            patched.index("RUN(b_pthread_createjoin_serial1, 0);"),
        )

    def test_qemu_cmdline_keeps_boot_observe_separate_from_guest_probe(self):
        mod = load_module()

        self.assertEqual(
            mod.qemu_cmdline("libcbench-musl", None),
            "tx.oscomp.observe=0 tx.oscomp.groups=libcbench-musl",
        )
        self.assertEqual(
            mod.qemu_cmdline("libcbench-musl", 12000),
            "tx.oscomp.observe_threshold=12000 tx.oscomp.groups=libcbench-musl",
        )

    def test_libcbench_patch_can_select_benchmark_without_observe_probe(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_pthread_createjoin_serial1, 0);
\tRUN(b_pthread_createjoin_serial2, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="pthread-serial2"
        )

        self.assertNotIn("syscall(333", patched)
        self.assertNotIn("#include <sys/syscall.h>", patched)
        self.assertNotIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertNotIn("RUN(b_pthread_createjoin_serial1, 0);", patched)
        self.assertIn("RUN(b_pthread_createjoin_serial2, 0);", patched)

    def test_libcbench_patch_can_select_stdio_group(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_stdio_putcgetc, 0);
\tRUN(b_stdio_putcgetc_unlocked, 0);
\tRUN(b_regex_compile, "(a|b|c)*d*b");
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="stdio"
        )

        self.assertNotIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertIn("RUN(b_stdio_putcgetc, 0);", patched)
        self.assertIn("RUN(b_stdio_putcgetc_unlocked, 0);", patched)
        self.assertNotIn('RUN(b_regex_compile, "(a|b|c)*d*b");', patched)

    def test_libcbench_patch_can_select_mm_io_pthread_group_with_trace_bracket(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_string_strlen, 0);
\tRUN(b_pthread_createjoin_serial1, 0);
\tRUN(b_stdio_putcgetc, 0);
\tRUN(b_regex_compile, "(a|b|c)*d*b");
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="mm-io-pthread", trace_bracket=True
        )

        self.assertIn("syscall(334);", patched)
        self.assertIn("syscall(335);", patched)
        self.assertIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertIn("RUN(b_stdio_putcgetc, 0);", patched)
        self.assertIn("RUN(b_pthread_createjoin_serial1, 0);", patched)
        self.assertNotIn("RUN(b_string_strlen, 0);", patched)
        self.assertNotIn('RUN(b_regex_compile, "(a|b|c)*d*b");', patched)

    def test_libcbench_patch_can_select_regex_group(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_stdio_putcgetc, 0);
\tRUN(b_regex_compile, "(a|b|c)*d*b");
\tRUN(b_regex_search, "(a|b|c)*d*b");
\tRUN(b_regex_search, "a{25}b");
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="regex"
        )

        self.assertNotIn("RUN(b_stdio_putcgetc, 0);", patched)
        self.assertIn('RUN(b_regex_compile, "(a|b|c)*d*b");', patched)
        self.assertIn('RUN(b_regex_search, "(a|b|c)*d*b");', patched)
        self.assertIn('RUN(b_regex_search, "a{25}b");', patched)

    def test_libcbench_patch_can_select_malloc_group(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_malloc_bubble, 0);
\tRUN(b_malloc_thread_local, 0);
\tRUN(b_string_strlen, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="malloc"
        )

        self.assertIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertIn("RUN(b_malloc_bubble, 0);", patched)
        self.assertIn("RUN(b_malloc_thread_local, 0);", patched)
        self.assertNotIn("RUN(b_string_strlen, 0);", patched)

    def test_libcbench_patch_can_select_malloc_vm_group(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_malloc_bubble, 0);
\tRUN(b_malloc_tiny1, 0);
\tRUN(b_malloc_big1, 0);
\tRUN(b_malloc_big2, 0);
\tRUN(b_malloc_thread_local, 0);
\tRUN(b_string_strlen, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="malloc-vm"
        )

        self.assertIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertIn("RUN(b_malloc_bubble, 0);", patched)
        self.assertIn("RUN(b_malloc_big1, 0);", patched)
        self.assertIn("RUN(b_malloc_big2, 0);", patched)
        self.assertNotIn("RUN(b_malloc_tiny1, 0);", patched)
        self.assertNotIn("RUN(b_malloc_thread_local, 0);", patched)
        self.assertNotIn("RUN(b_string_strlen, 0);", patched)

    def test_libcbench_patch_can_select_single_malloc_vm_benchmark(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_malloc_bubble, 0);
\tRUN(b_malloc_big1, 0);
\tRUN(b_malloc_big2, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="malloc-big2"
        )

        self.assertNotIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertNotIn("RUN(b_malloc_bubble, 0);", patched)
        self.assertNotIn("RUN(b_malloc_big1, 0);", patched)
        self.assertIn("RUN(b_malloc_big2, 0);", patched)

    def test_libcbench_patch_can_select_single_stdio_benchmark(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_stdio_putcgetc, 0);
\tRUN(b_stdio_putcgetc_unlocked, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="stdio-putcgetc-unlocked"
        )

        self.assertNotIn("RUN(b_stdio_putcgetc, 0);", patched)
        self.assertIn("RUN(b_stdio_putcgetc_unlocked, 0);", patched)

    def test_libcbench_patch_can_bracket_selected_benchmark_trace(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_pthread_createjoin_serial1, 0);
\tRUN(b_pthread_createjoin_minimal2, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source,
            threshold=None,
            phase="pthread",
            only="pthread-minimal2",
            trace_bracket=True,
        )

        self.assertIn("#include <sys/syscall.h>", patched)
        self.assertLess(
            patched.index("syscall(334);"),
            patched.index("RUN(b_pthread_createjoin_minimal2, 0);"),
        )
        self.assertGreater(
            patched.index("syscall(335);"),
            patched.index("RUN(b_pthread_createjoin_minimal2, 0);"),
        )
        self.assertNotIn("RUN(b_pthread_createjoin_serial1, 0);", patched)

    def test_libcbench_patch_can_bracket_only_timed_body(self):
        mod = load_module()
        source = """#include <unistd.h>
int run_bench(const char *label, size_t (*bench)(void *), void *params)
{
\tstruct timespec tv0;
\tputs(label);
\tclock_gettime(CLOCK_REALTIME, &tv0);
\tbench(params);
\tprint_stats(tv0);
\texit(0);
}
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_malloc_bubble, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source,
            threshold=None,
            phase="pthread",
            only="malloc-sparse",
            body_trace_bracket=True,
        )

        self.assertIn("#include <sys/syscall.h>", patched)
        self.assertLess(patched.index("clock_gettime"), patched.index("syscall(334);"))
        self.assertLess(patched.index("syscall(334);"), patched.index("bench(params);"))
        self.assertLess(patched.index("bench(params);"), patched.index("syscall(335);"))
        self.assertLess(patched.index("syscall(335);"), patched.index("print_stats(tv0);"))
        self.assertIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertNotIn("RUN(b_malloc_bubble, 0);", patched)

    def test_libcbench_body_bracket_requires_single_benchmark(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
}
"""

        with self.assertRaisesRegex(ValueError, "single --libcbench-only benchmark"):
            mod.patch_libcbench_main(
                source,
                threshold=None,
                phase="pthread",
                only="malloc-vm",
                body_trace_bracket=True,
            )

    def test_libcbench_only_pthread_replaces_main_body(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_pthread_createjoin_serial1, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=30000, phase="pthread", only="pthread"
        )

        self.assertNotIn("RUN(b_malloc_sparse, 0);", patched)
        self.assertIn("RUN(b_pthread_createjoin_serial1, 0);", patched)
        self.assertIn("RUN(b_pthread_createjoin_serial2, 0);", patched)
        self.assertIn("syscall(333, 30000);", patched)

    def test_libcbench_only_pthread_serial1_keeps_one_benchmark(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_malloc_sparse, 0);
\tRUN(b_pthread_createjoin_serial1, 0);
\tRUN(b_pthread_createjoin_serial2, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=5000, phase="pthread", only="pthread-serial1"
        )

        self.assertIn("RUN(b_pthread_createjoin_serial1, 0);", patched)
        self.assertNotIn("RUN(b_pthread_createjoin_serial2, 0);", patched)
        self.assertIn("syscall(333, 5000);", patched)

    def test_libcbench_only_pthread_minimal2_keeps_minimal_benchmark(self):
        mod = load_module()
        source = """#include <unistd.h>
#define RUN(a, b) run_bench(#a, a, b)
int main()
{
\tRUN(b_pthread_createjoin_serial2, 0);
\tRUN(b_pthread_createjoin_minimal2, 0);
}
"""

        patched = mod.patch_libcbench_main(
            source, threshold=None, phase="pthread", only="pthread-minimal2"
        )

        self.assertNotIn("syscall(333", patched)
        self.assertNotIn("RUN(b_pthread_createjoin_serial2, 0);", patched)
        self.assertIn("RUN(b_pthread_createjoin_minimal2, 0);", patched)

    def test_libcbench_pthread_patch_can_shrink_outer_batch_loop(self):
        mod = load_module()
        source = """size_t b_pthread_createjoin_minimal2(void *dummy)
{
\tsize_t i, j;
\tfor (j=0; j<50; j++) {
\t\tfor (i=0; i<50; i++) ;
\t}
}
"""

        patched = mod.patch_libcbench_pthread(
            source, outer_repeat=1, inner_repeat=None, serial_repeat=None
        )

        self.assertIn("for (j=0; j<1; j++)", patched)
        self.assertNotIn("for (j=0; j<50; j++)", patched)

    def test_libcbench_pthread_patch_can_shrink_inner_create_join_loop(self):
        mod = load_module()
        source = """size_t b_pthread_createjoin_minimal2(void *dummy)
{
\tsize_t i, j;
\tpthread_t td[50];
\tfor (j=0; j<50; j++) {
\t\tfor (i=0; i<sizeof td/sizeof *td; i++)
\t\t\tpthread_create(td+i, 0, emptyfunc, 0);
\t\tfor (i=0; i<sizeof td/sizeof *td; i++)
\t\t\tpthread_join(td[i], &dummy);
\t}
}
"""

        patched = mod.patch_libcbench_pthread(
            source, outer_repeat=None, inner_repeat=5, serial_repeat=None
        )

        self.assertIn("for (i=0; i<5; i++)", patched)
        self.assertNotIn("sizeof td/sizeof *td", patched)

    def test_libcbench_pthread_patch_can_shrink_serial_loop(self):
        mod = load_module()
        source = """size_t b_pthread_createjoin_minimal1(void *dummy)
{
\tsize_t i;
\tfor (i=0; i<2500; i++) {
\t\tpthread_create(&td, &a, emptyfunc, 0);
\t\tpthread_join(td, &dummy);
\t}
}
"""

        patched = mod.patch_libcbench_pthread(
            source, outer_repeat=None, inner_repeat=None, serial_repeat=50
        )

        self.assertIn("for (i=0; i<50; i++)", patched)
        self.assertNotIn("for (i=0; i<2500; i++)", patched)


if __name__ == "__main__":
    unittest.main()
