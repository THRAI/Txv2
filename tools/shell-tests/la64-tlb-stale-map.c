#define _GNU_SOURCE

#include <errno.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define TARGET_STACK_BYTES (256u * 1024u)

enum target_phase {
    TARGET_PHASE_STARTING = -1,
    TARGET_PHASE_IDLE = 0,
    TARGET_PHASE_ARMED = 1,
    TARGET_PHASE_PRIMED = 2,
    TARGET_PHASE_EXECUTE = 3,
    TARGET_PHASE_DONE = 4,
    TARGET_PHASE_STOP = 5,
};

enum target_status {
    TARGET_STATUS_OK = 0,
    TARGET_STATUS_PIN_FAILED = 40,
    TARGET_STATUS_PRIME_FAILED = 41,
    TARGET_STATUS_STALE_READ = 44,
    TARGET_STATUS_UNEXPECTED_READ = 45,
};

struct target_context {
    _Atomic uintptr_t address;
    _Atomic uint64_t old_marker;
    _Atomic uint64_t new_marker;
    _Atomic unsigned epoch;
    _Atomic int phase;
    _Atomic int status;
};

struct target_task {
    pid_t pid;
    void *stack;
};

static _Atomic unsigned validated_writes;
static _Atomic unsigned stale_writes;
static _Atomic unsigned stale_reads;
static _Atomic unsigned errors;

static int pin_current_task(unsigned cpu)
{
    cpu_set_t requested;
    cpu_set_t observed;
    int index;

    CPU_ZERO(&requested);
    CPU_SET(cpu, &requested);
    if (sched_setaffinity(0, sizeof(requested), &requested) != 0)
        return -errno;

    CPU_ZERO(&observed);
    if (sched_getaffinity(0, sizeof(observed), &observed) < 0)
        return -errno;
    if (!CPU_ISSET(cpu, &observed))
        return -EIO;
    for (index = 0; index < CPU_SETSIZE; ++index) {
        if (index != (int)cpu && CPU_ISSET(index, &observed))
            return -EIO;
    }
    return 0;
}

static uint64_t replacement_write_marker(uint64_t new_marker)
{
    return new_marker ^ UINT64_C(0x00ff00ff00ff00ff);
}

static void print_epoch_marker(unsigned epoch, const char *stage)
{
    printf("TLBORACLE:EPOCH:%u:%s\n", epoch, stage);
    fflush(stdout);
}

static int wait_for_phase(const struct target_context *context, int expected)
{
    int phase;

    while ((phase = atomic_load_explicit(&context->phase, memory_order_acquire)) !=
           expected) {
        if (phase == TARGET_PHASE_DONE && expected != TARGET_PHASE_DONE)
            return phase;
        sched_yield();
    }
    return phase;
}

static int target_main(void *argument)
{
    struct target_context *context = argument;
    int pin_result;

    pin_result = pin_current_task(1);
    if (pin_result != 0) {
        atomic_store_explicit(&context->status, -pin_result, memory_order_release);
        atomic_store_explicit(&context->phase, TARGET_PHASE_DONE, memory_order_release);
        _exit(TARGET_STATUS_PIN_FAILED);
    }

    atomic_store_explicit(&context->status, TARGET_STATUS_OK, memory_order_release);
    atomic_store_explicit(&context->phase, TARGET_PHASE_IDLE, memory_order_release);

    for (;;) {
        int phase = atomic_load_explicit(&context->phase, memory_order_acquire);

        if (phase == TARGET_PHASE_STOP)
            _exit(0);
        if (phase != TARGET_PHASE_ARMED) {
            sched_yield();
            continue;
        }

        volatile uint64_t *address = (volatile uint64_t *)atomic_load_explicit(
            &context->address, memory_order_acquire);
        uint64_t old_marker =
            atomic_load_explicit(&context->old_marker, memory_order_acquire);
        uint64_t new_marker =
            atomic_load_explicit(&context->new_marker, memory_order_acquire);
        uint64_t write_marker = replacement_write_marker(new_marker);
        uint64_t value = *address;

        if (value != old_marker) {
            atomic_store_explicit(&context->status,
                                  TARGET_STATUS_PRIME_FAILED,
                                  memory_order_release);
            atomic_store_explicit(&context->phase,
                                  TARGET_PHASE_DONE,
                                  memory_order_release);
            continue;
        }

        atomic_store_explicit(&context->status, TARGET_STATUS_OK, memory_order_release);
        atomic_store_explicit(&context->phase, TARGET_PHASE_PRIMED, memory_order_release);

        for (;;) {
            phase = atomic_load_explicit(&context->phase, memory_order_acquire);
            if (phase != TARGET_PHASE_PRIMED)
                break;
            sched_yield();
        }
        if (phase == TARGET_PHASE_STOP)
            _exit(0);
        if (phase != TARGET_PHASE_EXECUTE) {
            atomic_store_explicit(&context->status,
                                  TARGET_STATUS_UNEXPECTED_READ,
                                  memory_order_release);
            atomic_store_explicit(&context->phase,
                                  TARGET_PHASE_DONE,
                                  memory_order_release);
            continue;
        }

        *address = write_marker;
        value = *address;
        if (value == old_marker) {
            atomic_fetch_add_explicit(&stale_reads, 1, memory_order_relaxed);
            atomic_store_explicit(&context->status,
                                  TARGET_STATUS_STALE_READ,
                                  memory_order_release);
        } else if (value != write_marker) {
            atomic_fetch_add_explicit(&errors, 1, memory_order_relaxed);
            atomic_store_explicit(&context->status,
                                  TARGET_STATUS_UNEXPECTED_READ,
                                  memory_order_release);
        } else {
            atomic_store_explicit(&context->status, TARGET_STATUS_OK, memory_order_release);
        }
        atomic_store_explicit(&context->phase, TARGET_PHASE_DONE, memory_order_release);
    }
}

static int start_target_task(struct target_task *task,
                             int (*entry)(void *),
                             struct target_context *context)
{
    void *stack_top;

    task->stack = mmap(NULL,
                       TARGET_STACK_BYTES,
                       PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS,
                       -1,
                       0);
    if (task->stack == MAP_FAILED)
        return -1;
    stack_top = (char *)task->stack + TARGET_STACK_BYTES;
    task->pid = clone(entry, stack_top, CLONE_VM | SIGCHLD, context);
    if (task->pid < 0) {
        munmap(task->stack, TARGET_STACK_BYTES);
        task->stack = MAP_FAILED;
        return -1;
    }
    return 0;
}

static int stop_target_task(struct target_task *task, struct target_context *context)
{
    int status;
    pid_t waited;

    if (task->stack == MAP_FAILED)
        return 0;
    atomic_store_explicit(&context->phase, TARGET_PHASE_STOP, memory_order_release);
    do {
        waited = waitpid(task->pid, &status, 0);
    } while (waited < 0 && errno == EINTR);
    if (waited != task->pid)
        return -1;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
        return -1;
    if (munmap(task->stack, TARGET_STACK_BYTES) != 0)
        return -1;
    task->stack = MAP_FAILED;
    return 0;
}

static int create_epoch_memfd(const char *name, size_t page_size)
{
    int fd = memfd_create(name, MFD_CLOEXEC);

    if (fd < 0)
        return -1;
    if (ftruncate(fd, (off_t)page_size) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int run_replacement_epoch(struct target_context *context,
                                 size_t page_size,
                                 unsigned epoch)
{
    uint64_t old_marker = UINT64_C(0xa11a000000000000) | epoch;
    uint64_t new_marker = UINT64_C(0xb22b000000000000) | epoch;
    uint64_t write_marker = replacement_write_marker(new_marker);
    int fd_a = -1;
    int fd_b = -1;
    volatile uint64_t *keep_a = MAP_FAILED;
    volatile uint64_t *va_x = MAP_FAILED;
    volatile uint64_t *source_b = MAP_FAILED;
    void *moved;
    int phase;
    int status;
    int result = -1;

    print_epoch_marker(epoch, "START");
    fd_a = create_epoch_memfd("tx-tlb-a", page_size);
    fd_b = create_epoch_memfd("tx-tlb-b", page_size);
    if (fd_a < 0 || fd_b < 0)
        goto out;

    keep_a = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd_a, 0);
    va_x = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd_a, 0);
    source_b = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd_b, 0);
    if (keep_a == MAP_FAILED || va_x == MAP_FAILED || source_b == MAP_FAILED)
        goto out;

    *keep_a = old_marker;
    *source_b = new_marker;
    if (*va_x != old_marker)
        goto out;
    print_epoch_marker(epoch, "MAPPED");

    atomic_store_explicit(&context->address, (uintptr_t)va_x, memory_order_release);
    atomic_store_explicit(&context->old_marker, old_marker, memory_order_release);
    atomic_store_explicit(&context->new_marker, new_marker, memory_order_release);
    atomic_store_explicit(&context->epoch, epoch, memory_order_release);
    atomic_store_explicit(&context->status, TARGET_STATUS_OK, memory_order_release);
    atomic_store_explicit(&context->phase, TARGET_PHASE_ARMED, memory_order_release);

    phase = wait_for_phase(context, TARGET_PHASE_PRIMED);
    if (phase != TARGET_PHASE_PRIMED) {
        errno = EIO;
        goto out;
    }
    status = atomic_load_explicit(&context->status, memory_order_acquire);
    if (status != TARGET_STATUS_OK) {
        errno = EIO;
        goto out;
    }
    print_epoch_marker(epoch, "PRIMED");

    if (mprotect((void *)va_x, page_size, PROT_READ) != 0)
        goto out;
    print_epoch_marker(epoch, "MPROTECT");

    moved = mremap((void *)source_b,
                   page_size,
                   page_size,
                   MREMAP_MAYMOVE | MREMAP_FIXED,
                   (void *)va_x);
    if (moved == MAP_FAILED || moved != (void *)va_x)
        goto out;
    source_b = MAP_FAILED;
    print_epoch_marker(epoch, "MREMAP");

    if (*keep_a != old_marker || *va_x != new_marker) {
        errno = EIO;
        goto out;
    }

    atomic_store_explicit(&context->phase, TARGET_PHASE_EXECUTE, memory_order_release);
    print_epoch_marker(epoch, "RELEASED");
    phase = wait_for_phase(context, TARGET_PHASE_DONE);
    if (phase != TARGET_PHASE_DONE) {
        errno = EIO;
        goto out;
    }
    status = atomic_load_explicit(&context->status, memory_order_acquire);
    if (status != TARGET_STATUS_OK)
        atomic_fetch_add_explicit(&errors, 1, memory_order_relaxed);
    else {
        if (*keep_a != old_marker)
            atomic_fetch_add_explicit(&stale_writes, 1, memory_order_relaxed);
        if (*va_x == write_marker)
            atomic_fetch_add_explicit(&validated_writes, 1, memory_order_relaxed);
        else
            atomic_fetch_add_explicit(&errors, 1, memory_order_relaxed);
    }
    print_epoch_marker(epoch, "DONE");
    atomic_store_explicit(&context->phase, TARGET_PHASE_IDLE, memory_order_release);
    result = 0;

out:
    if (keep_a != MAP_FAILED)
        munmap((void *)keep_a, page_size);
    if (va_x != MAP_FAILED)
        munmap((void *)va_x, page_size);
    if (source_b != MAP_FAILED)
        munmap((void *)source_b, page_size);
    if (fd_a >= 0)
        close(fd_a);
    if (fd_b >= 0)
        close(fd_b);
    return result;
}

static unsigned parse_epochs(int argc, char **argv)
{
    char *end = NULL;
    unsigned long value;

    if (argc < 2)
        return 32;
    errno = 0;
    value = strtoul(argv[1], &end, 10);
    if (errno != 0 || end == argv[1] || *end != '\0' || value == 0 || value > 100000)
        return 0;
    return (unsigned)value;
}

int main(int argc, char **argv)
{
    struct target_context context = {
        .address = ATOMIC_VAR_INIT(0),
        .old_marker = ATOMIC_VAR_INIT(0),
        .new_marker = ATOMIC_VAR_INIT(0),
        .epoch = ATOMIC_VAR_INIT(0),
        .phase = ATOMIC_VAR_INIT(TARGET_PHASE_STARTING),
        .status = ATOMIC_VAR_INIT(TARGET_STATUS_OK),
    };
    struct target_task task = {.pid = -1, .stack = MAP_FAILED};
    unsigned epochs = parse_epochs(argc, argv);
    unsigned epoch;
    unsigned validated;
    unsigned stale;
    unsigned reads;
    unsigned error_count;
    long page_size;
    int rc;
    int phase;

    if (epochs == 0) {
        fprintf(stderr, "TLBORACLE:FAIL:invalid-epochs\n");
        return 2;
    }
    page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        fprintf(stderr, "TLBORACLE:FAIL:setup:errno=%d\n", errno);
        return 2;
    }
    rc = pin_current_task(0);
    if (rc != 0) {
        fprintf(stderr, "TLBORACLE:FAIL:pin-controller:errno=%d\n", -rc);
        return 2;
    }

    printf("TLBORACLE:BEGIN:epochs=%u:controller_cpu=0:target_cpu=1:shared_mm=1\n", epochs);
    fflush(stdout);
    if (start_target_task(&task, target_main, &context) != 0) {
        fprintf(stderr, "TLBORACLE:FAIL:start-target:errno=%d\n", errno);
        return 2;
    }
    phase = wait_for_phase(&context, TARGET_PHASE_IDLE);
    if (phase != TARGET_PHASE_IDLE) {
        fprintf(stderr, "TLBORACLE:FAIL:target-start:phase=%d:status=%d\n",
                phase,
                atomic_load_explicit(&context.status, memory_order_acquire));
        stop_target_task(&task, &context);
        return 2;
    }

    for (epoch = 1; epoch <= epochs; ++epoch) {
        if (run_replacement_epoch(&context, (size_t)page_size, epoch) != 0) {
            fprintf(stderr, "TLBORACLE:FAIL:protect-replace-epoch=%u:errno=%d\n", epoch, errno);
            atomic_fetch_add_explicit(&errors, 1, memory_order_relaxed);
            break;
        }
        if (epoch == epochs || epoch % 8 == 0) {
            printf("TLBORACLE:EPOCH:%u:PASS\n", epoch);
            fflush(stdout);
        }
    }

    if (stop_target_task(&task, &context) != 0)
        atomic_fetch_add_explicit(&errors, 1, memory_order_relaxed);

    validated = atomic_load_explicit(&validated_writes, memory_order_acquire);
    stale = atomic_load_explicit(&stale_writes, memory_order_acquire);
    reads = atomic_load_explicit(&stale_reads, memory_order_acquire);
    error_count = atomic_load_explicit(&errors, memory_order_acquire);
    printf("TLBORACLE:RESULT:epochs=%u:validated_writes=%u:stale_writes=%u:stale_reads=%u:errors=%u\n",
           epochs,
           validated,
           stale,
           reads,
           error_count);
    if (epoch == epochs + 1 && validated == epochs && stale == 0 && reads == 0 &&
        error_count == 0) {
        printf("TLBORACLE:PASS\n");
        return 0;
    }
    printf("TLBORACLE:FAIL:counts\n");
    return 1;
}
