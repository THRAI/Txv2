typedef unsigned char u8;
typedef unsigned int u32;
typedef unsigned long u64;
typedef long i64;

enum {
    SYS_CLOSE = 57,
    SYS_PIPE2 = 59,
    SYS_READ = 63,
    SYS_WRITE = 64,
    SYS_FUTEX = 98,
    SYS_NANOSLEEP = 101,
    SYS_SCHED_SETAFFINITY = 122,
    SYS_SCHED_YIELD = 124,
    SYS_GETCPU = 168,
    SYS_GETPID = 172,
    SYS_CLONE = 220,
    SYS_EXIT = 93,
    CLONE_VM = 0x00000100,
    CLONE_SIGHAND = 0x00000800,
    CLONE_THREAD = 0x00010000,
    FUTEX_WAIT = 0,
    FUTEX_WAKE = 1,
    WORKERS = 4,
    STACK_BYTES = 64 * 1024,
    STACK_CALLER_HEADROOM = 128,
    SPIN_LIMIT = 100000000,
    PIPE_ROUNDS = 8,
    TIMER_SLEEPS = 8,
};

struct timespec64 {
    i64 tv_sec;
    i64 tv_nsec;
};

struct witness_state {
    volatile u32 done;
    volatile u32 ready;
    volatile u32 start;
    volatile u32 spawned;
    volatile u32 errors;
    volatile u32 seen_mask;
    volatile u32 cpu_slot[WORKERS];
};

struct case_metrics {
    volatile u32 affinity_migrations;
    volatile u32 affinity_errors;
    volatile u32 affinity_mask;
    volatile u32 pipe_rounds;
    volatile u32 pipe_errors;
    volatile u32 pipe_reader_mask;
    volatile u32 pipe_writer_mask;
    volatile u32 timer_sleeps;
    volatile u32 timer_errors;
    volatile u32 pthread_joins;
    volatile u32 futex_wakes;
    volatile u32 pthread_errors;
};

static struct witness_state state;
static struct case_metrics metrics;
static const char *selected_case_name;
static u8 child_stacks[WORKERS][STACK_BYTES + STACK_CALLER_HEADROOM]
    __attribute__((aligned(16)));
static int pipe_fds[2];
static volatile u32 pipe_start;
static volatile u32 pipe_done;
static volatile u32 pipe_writer_claimed;
static volatile u32 pthread_waiting;
static volatile u32 pthread_done;

static long syscall3(long number, long a0, long a1, long a2)
{
    register long syscall_number asm("a7") = number;
    register long first asm("a0") = a0;
    register long second asm("a1") = a1;
    register long third asm("a2") = a2;
    asm volatile(
        "ecall"
        : "+r"(first)
        : "r"(second), "r"(third), "r"(syscall_number)
        : "memory");
    return first;
}

static long syscall4(long number, long a0, long a1, long a2, long a3)
{
    register long syscall_number asm("a7") = number;
    register long first asm("a0") = a0;
    register long second asm("a1") = a1;
    register long third asm("a2") = a2;
    register long fourth asm("a3") = a3;
    asm volatile(
        "ecall"
        : "+r"(first)
        : "r"(second), "r"(third), "r"(fourth), "r"(syscall_number)
        : "memory");
    return first;
}

static __attribute__((naked, noinline)) long raw_clone(long, long, long, long, long)
{
    asm volatile("li a7, 220\n\tecall\n\tret");
}

static void store_release(volatile u32 *address, u32 value)
{
    __sync_synchronize();
    *address = value;
}

static u32 load_acquire(volatile u32 *address)
{
    u32 value = *address;
    __sync_synchronize();
    return value;
}

static int poll_until(volatile u32 *address, u32 value)
{
    for (u64 spin = 0; spin < SPIN_LIMIT; spin++) {
        if (load_acquire(address) == value) return 1;
        if ((spin & 0x3ff) == 0) {
            (void)syscall3(SYS_GETPID, 0, 0, 0);
        }
        asm volatile("nop" ::: "memory");
    }
    return 0;
}

static u32 current_cpu_id(void)
{
    long cpu = syscall3(SYS_GETCPU, 0, 0, 0);
    if (cpu < 0) {
        return 0xffffffffu;
    }
    return (u32)cpu;
}

static int set_cpu_affinity(u64 mask)
{
    return syscall3(SYS_SCHED_SETAFFINITY, 0, sizeof(mask), (long)&mask) == 0;
}

static long futex_wait(volatile u32 *address, u32 expected)
{
    return syscall4(SYS_FUTEX, (long)address, FUTEX_WAIT, expected, 0);
}

static long futex_wake(volatile u32 *address, u32 count)
{
    return syscall4(SYS_FUTEX, (long)address, FUTEX_WAKE, count, 0);
}

static int nanosleep_ns(u64 nanos)
{
    struct timespec64 req;
    req.tv_sec = (i64)(nanos / 1000000000UL);
    req.tv_nsec = (i64)(nanos % 1000000000UL);
    return syscall3(SYS_NANOSLEEP, (long)&req, 0, 0) == 0;
}

static char *append_literal(char *out, const char *text)
{
    while (*text) *out++ = *text++;
    return out;
}

static char *append_decimal(char *out, u64 value)
{
    char digits[20];
    u64 count = 0;
    do {
        digits[count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);
    while (count != 0) *out++ = digits[--count];
    return out;
}

static char *append_hex(char *out, u64 value)
{
    static const char digits[] = "0123456789abcdef";
    *out++ = '0';
    *out++ = 'x';
    for (u64 shift = 16; shift != 0; shift--) {
        *out++ = digits[(value >> ((shift - 1) * 4)) & 0xf];
    }
    return out;
}

static int string_equal(const char *left, const char *right)
{
    while (*left && *right) {
        if (*left != *right) return 0;
        left++;
        right++;
    }
    return *left == *right;
}

static int string_has_prefix(const char *text, const char *prefix)
{
    while (*prefix) {
        if (*text != *prefix) return 0;
        text++;
        prefix++;
    }
    return 1;
}

static const char *stack_arg(u64 *stack, u64 index)
{
    return (const char *)(unsigned long)stack[1 + index];
}

static __attribute__((noreturn)) void fail(int status);

static const char *selected_case(u64 *stack)
{
    if (stack[0] >= 2) {
        const char *arg = stack_arg(stack, 1);
        if (string_equal(arg, "--witness-spread")) {
            return "static";
        }
        if (string_equal(arg, "--case")) {
            if (stack[0] < 3) fail(2);
            arg = stack_arg(stack, 2);
        } else if (string_has_prefix(arg, "--case=")) {
            arg += 7;
        } else {
            fail(2);
        }
        if (string_equal(arg, "static") || string_equal(arg, "movable")
            || string_equal(arg, "pipe") || string_equal(arg, "affinity")
            || string_equal(arg, "timer") || string_equal(arg, "pthread")
            || string_equal(arg, "mixed") || string_equal(arg, "stress")) {
            return arg;
        }
        fail(2);
    }
    return "static";
}

static __attribute__((noreturn)) void fail(int status)
{
    (void)syscall3(SYS_EXIT, status, 0, 0);
    for (;;) {}
}

static int string_equal(const char *left, const char *right);
static void mark_case_error(volatile u32 *field);
static int try_run_pipe_writer_case(void);

static int case_is(const char *name)
{
    return selected_case_name != 0 && string_equal(selected_case_name, name);
}

static void write_progress_marker(const char *phase, u32 slot, u64 value);
static void record_worker_cpu(u32 slot, u32 cpu);
static __attribute__((noreturn)) void worker_main(u32 slot)
{
    volatile u32 saved_slot = slot;
    __sync_fetch_and_add(&state.ready, 1);
    volatile u32 saved_cpu = current_cpu_id();
    record_worker_cpu(saved_slot, saved_cpu);
    for (;;) {
        if ((case_is("pipe") || case_is("mixed") || case_is("stress"))
            && try_run_pipe_writer_case()) {
            continue;
        }
        asm volatile("nop" ::: "memory");
    }
}

static void write_result_marker(const char *case_name, u32 pass, u32 spawned)
{
    char marker[512];
    char *out = marker;
    out = append_literal(out, "sched-smp:result case=");
    out = append_literal(out, case_name);
    out = append_literal(out, " pass=");
    out = append_decimal(out, pass);
    out = append_literal(out, " spawned=");
    out = append_decimal(out, spawned);
    out = append_literal(out, " ready=");
    out = append_decimal(out, state.ready);
    out = append_literal(out, " done=");
    out = append_decimal(out, state.done);
    out = append_literal(out, " errors=");
    out = append_decimal(out, state.errors);
    out = append_literal(out, " workers=");
    out = append_decimal(out, WORKERS);
    out = append_literal(out, " seen-mask=");
    out = append_hex(out, state.seen_mask);
    out = append_literal(out, " cpu0=");
    out = append_decimal(out, state.cpu_slot[0]);
    out = append_literal(out, " cpu1=");
    out = append_decimal(out, state.cpu_slot[1]);
    out = append_literal(out, " cpu2=");
    out = append_decimal(out, state.cpu_slot[2]);
    out = append_literal(out, " cpu3=");
    out = append_decimal(out, state.cpu_slot[3]);
    out = append_literal(out, " affinity-migrations=");
    out = append_decimal(out, metrics.affinity_migrations);
    out = append_literal(out, " affinity-errors=");
    out = append_decimal(out, metrics.affinity_errors);
    out = append_literal(out, " affinity-mask=");
    out = append_hex(out, metrics.affinity_mask);
    out = append_literal(out, " pipe-rounds=");
    out = append_decimal(out, metrics.pipe_rounds);
    out = append_literal(out, " pipe-errors=");
    out = append_decimal(out, metrics.pipe_errors);
    out = append_literal(out, " pipe-reader-mask=");
    out = append_hex(out, metrics.pipe_reader_mask);
    out = append_literal(out, " pipe-writer-mask=");
    out = append_hex(out, metrics.pipe_writer_mask);
    out = append_literal(out, " timer-sleeps=");
    out = append_decimal(out, metrics.timer_sleeps);
    out = append_literal(out, " timer-errors=");
    out = append_decimal(out, metrics.timer_errors);
    out = append_literal(out, " pthread-joins=");
    out = append_decimal(out, metrics.pthread_joins);
    out = append_literal(out, " futex-wakes=");
    out = append_decimal(out, metrics.futex_wakes);
    out = append_literal(out, " pthread-errors=");
    out = append_decimal(out, metrics.pthread_errors);
    out = append_literal(out, "\n");
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
}

static void write_begin_marker(const char *case_name)
{
    char marker[64];
    char *out = marker;
    out = append_literal(out, "sched-smp:begin case=");
    out = append_literal(out, case_name);
    out = append_literal(out, " workers=4\n");
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
}

static void write_progress_marker(const char *phase, u32 slot, u64 value)
{
    char marker[128];
    char *out = marker;
    out = append_literal(out, "sched-smp:");
    out = append_literal(out, phase);
    out = append_literal(out, " slot=");
    out = append_decimal(out, slot);
    out = append_literal(out, " value=");
    out = append_decimal(out, value);
    out = append_literal(out, "\n");
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
}

static __attribute__((noinline)) void record_worker_cpu(u32 slot, u32 cpu)
{
    if (slot >= WORKERS) {
        store_release(&state.errors, 1);
        __sync_fetch_and_add(&state.done, 1);
        return;
    }

    if (cpu < 32) {
        __sync_fetch_and_or(&state.seen_mask, 1u << cpu);
    } else {
        store_release(&state.errors, 1);
    }

    state.cpu_slot[slot] = cpu;
    __sync_fetch_and_add(&state.done, 1);
}

static void mark_case_error(volatile u32 *field)
{
    __sync_fetch_and_add(field, 1);
    store_release(&state.errors, 1);
}

static int try_run_pipe_writer_case(void)
{
    if (load_acquire(&pipe_start) != 1) {
        return 0;
    }
    if (!__sync_bool_compare_and_swap(&pipe_writer_claimed, 0, 1)) {
        return 0;
    }
    u32 cpu = current_cpu_id();
    if (cpu >= 32) {
        mark_case_error(&metrics.pipe_errors);
        return 1;
    }
    metrics.pipe_writer_mask = 1u << cpu;
    for (u32 round = 0; round < PIPE_ROUNDS; round++) {
        u8 byte = (u8)('a' + round);
        long written = syscall3(SYS_WRITE, pipe_fds[1], (long)&byte, 1);
        if (written != 1) {
            mark_case_error(&metrics.pipe_errors);
            break;
        }
    }
    store_release(&pipe_done, 1);
    return 1;
}

static void run_pipe_case(void)
{
    if (!set_cpu_affinity(0x1)) {
        mark_case_error(&metrics.pipe_errors);
        return;
    }
    u32 reader_cpu = current_cpu_id();
    if (reader_cpu >= 32) {
        mark_case_error(&metrics.pipe_errors);
        return;
    }
    metrics.pipe_reader_mask = 1u << reader_cpu;
    if (syscall3(SYS_PIPE2, (long)pipe_fds, 0, 0) != 0) {
        mark_case_error(&metrics.pipe_errors);
        return;
    }
    store_release(&pipe_writer_claimed, 0);

    store_release(&pipe_start, 1);
    for (u32 round = 0; round < PIPE_ROUNDS; round++) {
        u8 byte = 0;
        long read_count = syscall3(SYS_READ, pipe_fds[0], (long)&byte, 1);
        if (read_count != 1 || byte != (u8)('a' + round)) {
            mark_case_error(&metrics.pipe_errors);
            break;
        }
        __sync_fetch_and_add(&metrics.pipe_rounds, 1);
    }
    if (!poll_until(&pipe_done, 1)) {
        mark_case_error(&metrics.pipe_errors);
    }
    (void)syscall3(SYS_CLOSE, pipe_fds[0], 0, 0);
    (void)syscall3(SYS_CLOSE, pipe_fds[1], 0, 0);
}

static void run_affinity_case(void)
{
    u32 seen = 0;
    for (u32 target = 0; target < WORKERS; target++) {
        u64 mask = 1UL << target;
        if (!set_cpu_affinity(mask)) {
            mark_case_error(&metrics.affinity_errors);
            return;
        }
        for (u64 spin = 0; spin < SPIN_LIMIT; spin++) {
            (void)syscall3(SYS_SCHED_YIELD, 0, 0, 0);
            u32 cpu = current_cpu_id();
            if (cpu < 32) {
                seen |= 1u << cpu;
            }
            if (cpu == target) {
                __sync_fetch_and_add(&metrics.affinity_migrations, 1);
                break;
            }
            if (spin + 1 == SPIN_LIMIT) {
                mark_case_error(&metrics.affinity_errors);
                return;
            }
        }
    }
    metrics.affinity_mask = seen;
    if (!set_cpu_affinity(0xf)) {
        mark_case_error(&metrics.affinity_errors);
    }
}

static void run_timer_case(void)
{
    u32 seen = 0;
    for (u32 target = 0; target < WORKERS; target++) {
        if (!set_cpu_affinity(1UL << target)) {
            mark_case_error(&metrics.timer_errors);
            return;
        }
        for (u32 sleep = 0; sleep < 2; sleep++) {
            if (!nanosleep_ns(1 * 1000 * 1000)) {
                mark_case_error(&metrics.timer_errors);
                return;
            }
            __sync_fetch_and_add(&metrics.timer_sleeps, 1);
            u32 cpu = current_cpu_id();
            if (cpu < 32) {
                seen |= 1u << cpu;
            }
            if (cpu != target) {
                mark_case_error(&metrics.timer_errors);
                return;
            }
        }
    }
    metrics.affinity_mask = seen;
}

static void run_pthread_case(void)
{
    if (!set_cpu_affinity(0x1)) {
        mark_case_error(&metrics.pthread_errors);
        return;
    }
    store_release(&pthread_waiting, 0);
    store_release(&pthread_done, 0);
    store_release(&pthread_waiting, 1);
    long wait_result = futex_wait(&pthread_done, 1);
    if (wait_result != -11 /* EAGAIN */) {
        mark_case_error(&metrics.pthread_errors);
        return;
    }
    store_release(&pthread_done, 1);
    long woke = futex_wake(&pthread_done, 1);
    if (woke < 0) {
        mark_case_error(&metrics.pthread_errors);
        return;
    }
    __sync_fetch_and_add(&metrics.futex_wakes, 1);
    __sync_fetch_and_add(&metrics.pthread_joins, 1);
}

static void run_mixed_case(void)
{
    run_pipe_case();
    run_timer_case();
}

static void run_stress_case(void)
{
    run_pipe_case();
    run_affinity_case();
    run_timer_case();
    run_pthread_case();
}

static void run_case_extras(const char *case_name)
{
    if (string_equal(case_name, "pipe")) {
        run_pipe_case();
    } else if (string_equal(case_name, "affinity")) {
        run_affinity_case();
    } else if (string_equal(case_name, "timer")) {
        run_timer_case();
    } else if (string_equal(case_name, "pthread")) {
        run_pthread_case();
    } else if (string_equal(case_name, "mixed")) {
        run_mixed_case();
    } else if (string_equal(case_name, "stress")) {
        run_stress_case();
    }
}

static u64 current_stack_pointer(void)
{
    register u64 sp asm("sp");
    return sp;
}

static u32 worker_slot_from_stack(void)
{
    u64 sp = current_stack_pointer();
    for (u32 slot = 0; slot < WORKERS; slot++) {
        u64 low = (u64)&child_stacks[slot][0];
        u64 high = low + STACK_BYTES + STACK_CALLER_HEADROOM;
        if (sp >= low && sp <= high) return slot;
    }
    return WORKERS;
}

long vdso_phase5_main(u64 *stack)
{
    const char *case_name = selected_case(stack);
    selected_case_name = case_name;
    write_begin_marker(case_name);
    u32 spawned = 0;
    for (u32 slot = 0; slot < WORKERS; slot++) {
        write_progress_marker("clone-before", slot, spawned);
        long result = raw_clone(
            CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
            (long)(child_stacks[slot] + STACK_BYTES + STACK_CALLER_HEADROOM),
            0,
            0,
            0);
        if (result < 0) {
            write_progress_marker("clone-error", slot, 0);
            store_release(&state.errors, 1);
            break;
        }
        spawned += 1;
        store_release(&state.spawned, spawned);
        if (result == 0) {
            worker_main(worker_slot_from_stack());
        }
        write_progress_marker("clone-parent", slot, (u64)result);
    }

    if (!poll_until(&state.ready, spawned)) {
        store_release(&state.errors, 1);
    }
    store_release(&state.start, 1);
    if (!poll_until(&state.done, spawned)) {
        store_release(&state.errors, 1);
    }
    run_case_extras(case_name);
    if (state.errors != 0 || state.ready != WORKERS || spawned != WORKERS) {
        write_result_marker(case_name, 0, spawned);
        fail(1);
    }

    write_result_marker(case_name, 1, spawned);
    return 0;
}
