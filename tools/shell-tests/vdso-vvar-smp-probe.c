typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;
typedef long i64;

enum {
    AT_NULL = 0,
    AT_SYSINFO_EHDR = 33,
    PT_LOAD = 1,
    PT_DYNAMIC = 2,
    DT_NULL = 0,
    DT_HASH = 4,
    DT_STRTAB = 5,
    DT_SYMTAB = 6,
    DT_VERSYM = 0x6ffffff0,
    DT_VERDEF = 0x6ffffffc,
    SHN_UNDEF = 0,
    VER_FLG_BASE = 1,
    CLOCK_REALTIME = 0,
    SYS_WRITE = 64,
    SYS_CLOCK_SETTIME = 112,
    SYS_SCHED_SETAFFINITY = 122,
    SYS_SCHED_YIELD = 124,
    SYS_CLONE = 220,
    SYS_EXIT = 93,
    CLONE_VM = 0x00000100,
    CLONE_SIGHAND = 0x00000800,
    CLONE_THREAD = 0x00010000,
    WRITER_UPDATES = 1024,
    READER_READS = 500000,
    CHILD_STACK_BYTES = 64 * 1024,
    CHILD_STACK_CALLER_HEADROOM = 128,
    COORDINATION_SPIN_LIMIT = 100000000,
};

struct elf64_ehdr {
    u8 ident[16];
    u16 type;
    u16 machine;
    u32 version;
    u64 entry;
    u64 phoff;
    u64 shoff;
    u32 flags;
    u16 ehsize;
    u16 phentsize;
    u16 phnum;
    u16 shentsize;
    u16 shnum;
    u16 shstrndx;
};

struct elf64_phdr {
    u32 type;
    u32 flags;
    u64 offset;
    u64 vaddr;
    u64 paddr;
    u64 filesz;
    u64 memsz;
    u64 align;
};

struct elf64_dyn {
    i64 tag;
    u64 value;
};

struct elf64_sym {
    u32 name;
    u8 info;
    u8 other;
    u16 shndx;
    u64 value;
    u64 size;
};

struct elf64_verdef {
    u16 version;
    u16 flags;
    u16 index;
    u16 count;
    u32 hash;
    u32 aux;
    u32 next;
};

struct elf64_verdaux {
    u32 name;
    u32 next;
};

struct timespec64 {
    i64 tv_sec;
    i64 tv_nsec;
};

struct resolved_symbol {
    void *address;
};

struct shared_state {
    volatile u32 reader_ready;
    volatile u32 writer_started;
    volatile u32 writer_done;
    volatile u32 reader_done;
    volatile u32 writer_affinity;
    volatile u32 reader_affinity;
    volatile u32 writer_updates;
    volatile u32 reader_reads;
    volatile u32 reader_errors;
};

typedef int (*vdso_clock_gettime_fn)(int clock_id, struct timespec64 *value);

static struct shared_state state;
/* The clone return path has a caller frame before it enters writer_child. */
static u8 child_stack[CHILD_STACK_BYTES + CHILD_STACK_CALLER_HEADROOM]
    __attribute__((aligned(16)));
static volatile u64 reader_sink;

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

/* Returning directly keeps the child off the parent's call frame. */
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

/*
 * This probe isolates VVAR publication from the kernel's generic wait ABI.
 * Its two short-lived workers need only a start/done hand-off, so bounded
 * shared-memory polling is sufficient and avoids turning a futex/mailbox
 * regression into a false vDSO failure. The periodic sched_yield gives the
 * kernel one round trip to publish the cloned thread into its reactor.
 */
static int spin_until(volatile u32 *address, u32 value)
{
    for (u64 spin = 0; spin < COORDINATION_SPIN_LIMIT; spin++) {
        if (load_acquire(address) == value) return 1;
        if ((spin & 0xff) == 0) {
            (void)syscall3(SYS_SCHED_YIELD, 0, 0, 0);
        }
        asm volatile("nop" ::: "memory");
    }
    return 0;
}

static int set_cpu_affinity(u64 mask)
{
    return syscall3(SYS_SCHED_SETAFFINITY, 0, sizeof(mask), (long)&mask) == 0;
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

static int version_matches(
    const struct elf64_verdef *definition,
    u16 symbol_version,
    const char *version,
    const char *strings)
{
    symbol_version &= 0x7fff;
    for (;;) {
        if (!(definition->flags & VER_FLG_BASE)
            && (definition->index & 0x7fff) == symbol_version) {
            const struct elf64_verdaux *auxiliary =
                (const struct elf64_verdaux *)((const char *)definition + definition->aux);
            return string_equal(version, strings + auxiliary->name);
        }
        if (definition->next == 0) return 0;
        definition = (const struct elf64_verdef *)((const char *)definition + definition->next);
    }
}

static struct resolved_symbol find_versioned_symbol(
    void *base,
    const char *name,
    const char *version)
{
    struct resolved_symbol result = { 0 };
    const struct elf64_ehdr *header = base;
    const struct elf64_phdr *program =
        (const struct elf64_phdr *)((const char *)base + header->phoff);
    const struct elf64_dyn *dynamic = 0;
    u64 load_bias = 0;
    int have_load_bias = 0;

    for (u16 index = 0; index < header->phnum; index++) {
        if (program->type == PT_LOAD && !have_load_bias) {
            load_bias = (u64)(unsigned long)base + program->offset - program->vaddr;
            have_load_bias = 1;
        } else if (program->type == PT_DYNAMIC) {
            dynamic = (const struct elf64_dyn *)((const char *)base + program->offset);
        }
        program = (const struct elf64_phdr *)((const char *)program + header->phentsize);
    }
    if (!dynamic || !have_load_bias) return result;

    const char *strings = 0;
    const struct elf64_sym *symbols = 0;
    const u32 *hash = 0;
    const u16 *versions = 0;
    const struct elf64_verdef *definitions = 0;
    for (const struct elf64_dyn *entry = dynamic; entry->tag != DT_NULL; entry++) {
        const void *address = (const void *)(unsigned long)(load_bias + entry->value);
        switch (entry->tag) {
        case DT_STRTAB: strings = address; break;
        case DT_SYMTAB: symbols = address; break;
        case DT_HASH: hash = address; break;
        case DT_VERSYM: versions = address; break;
        case DT_VERDEF: definitions = address; break;
        }
    }
    if (!strings || !symbols || !hash || !versions || !definitions) return result;

    for (u32 index = 1; index < hash[1]; index++) {
        const struct elf64_sym *symbol = &symbols[index];
        if (symbol->shndx == SHN_UNDEF) continue;
        if (!string_equal(name, strings + symbol->name)) continue;
        if (!version_matches(definitions, versions[index], version, strings)) continue;
        result.address = (void *)(unsigned long)(load_bias + symbol->value);
        return result;
    }
    return result;
}

static int valid_timespec(const struct timespec64 *value)
{
    return value->tv_sec >= 0 && value->tv_nsec >= 0 && value->tv_nsec < 1000000000L;
}

static __attribute__((noreturn, noinline)) void writer_child(void)
{
    struct timespec64 value;
    if (!set_cpu_affinity(0x2)) {
        store_release(&state.writer_done, 1);
        (void)syscall3(SYS_EXIT, 1, 0, 0);
        for (;;) {}
    }
    store_release(&state.writer_affinity, 1);
    if (!spin_until(&state.reader_ready, 1)) {
        store_release(&state.writer_done, 1);
        (void)syscall3(SYS_EXIT, 1, 0, 0);
        for (;;) {}
    }

    for (u32 generation = 0; generation < WRITER_UPDATES; generation++) {
        value.tv_sec = 1800000000L + generation;
        value.tv_nsec = (i64)((generation * 1000003U) % 1000000000U);
        if (syscall3(SYS_CLOCK_SETTIME, CLOCK_REALTIME, (long)&value, 0) != 0) {
            store_release(&state.writer_done, 1);
            (void)syscall3(SYS_EXIT, 1, 0, 0);
            for (;;) {}
        }
        store_release(&state.writer_updates, generation + 1);
        if (generation == 0) {
            store_release(&state.writer_started, 1);
        }
    }
    store_release(&state.writer_done, 1);
    (void)syscall3(SYS_EXIT, 0, 0, 0);
    for (;;) {}
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

static void write_pass_marker(void)
{
    char marker[320];
    char *out = marker;
    out = append_literal(out, "vdso-phase5:vvar-smp=pass writer-updates=");
    out = append_decimal(out, state.writer_updates);
    out = append_literal(out, " reader-reads=");
    out = append_decimal(out, state.reader_reads);
    out = append_literal(out, " writer-mask=");
    out = append_hex(out, 0x2);
    out = append_literal(out, " reader-mask=");
    out = append_hex(out, 0x1);
    out = append_literal(out, " writer-affinity=ok reader-affinity=ok reader-path=direct-vdso writer-done=1 reader-done=1 reader-errors=0\n");
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
}

long vdso_phase5_main(u64 *stack)
{
    u64 *cursor = stack + 1 + stack[0] + 1;
    while (*cursor++) {}
    u64 auxv = 0;
    while (cursor[0] != AT_NULL) {
        if (cursor[0] == AT_SYSINFO_EHDR) auxv = cursor[1];
        cursor += 2;
    }
    if (auxv == 0) return 1;

    struct resolved_symbol clock_gettime = find_versioned_symbol(
        (void *)(unsigned long)auxv, "__vdso_clock_gettime", "LINUX_4.15");
    vdso_clock_gettime_fn vdso_clock_gettime = (vdso_clock_gettime_fn)clock_gettime.address;
    long clone_result = raw_clone(
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        (long)(child_stack + CHILD_STACK_BYTES),
        0,
        0,
        0);
    if (clone_result < 0) return 1;
    if (clone_result == 0) writer_child();

    if (set_cpu_affinity(0x1)) store_release(&state.reader_affinity, 1);
    store_release(&state.reader_ready, 1);
    (void)spin_until(&state.writer_started, 1);

    if (state.reader_affinity == 0 || !vdso_clock_gettime || state.writer_started == 0) {
        store_release(&state.reader_errors, 1);
    } else {
        for (u32 iteration = 0; iteration < READER_READS; iteration++) {
            struct timespec64 value;
            if (vdso_clock_gettime(CLOCK_REALTIME, &value) != 0 || !valid_timespec(&value)) {
                store_release(&state.reader_errors, 1);
                break;
            }
            reader_sink = (u64)value.tv_sec ^ (u64)value.tv_nsec;
            store_release(&state.reader_reads, iteration + 1);
        }
    }
    store_release(&state.reader_done, 1);
    if (!spin_until(&state.writer_done, 1)) return 1;

    if (state.writer_affinity != 1 || state.reader_affinity != 1
        || state.writer_updates != WRITER_UPDATES || state.reader_reads != READER_READS
        || state.reader_errors != 0 || state.writer_done != 1 || state.reader_done != 1) {
        return 1;
    }
    write_pass_marker();
    return 0;
}
