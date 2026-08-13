typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;
typedef int i32;
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
    CLOCK_MONOTONIC = 1,
    FALLBACK_CLOCK_ID = 0x40000000,
    ENOSYS_VALUE = 38,
    EINVAL_VALUE = 22,
    SYS_WRITE = 64,
    SYS_CLOCK_SETTIME = 112,
    SYS_CLOCK_GETTIME = 113,
    SYS_KILL = 129,
    SYS_RT_SIGACTION = 134,
    SYS_GETPID = 172,
    SIGUSR1 = 10,
    SIGSET_SIZE = 8,
    VVAR_STRESS_UPDATES = 1024,
    VVAR_STRESS_READS = 500000,
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

struct elf64_shdr {
    u32 name;
    u32 type;
    u64 flags;
    u64 addr;
    u64 offset;
    u64 size;
    u32 link;
    u32 info;
    u64 addralign;
    u64 entsize;
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

struct rt_sigaction64 {
    u64 handler;
    u64 flags;
    u64 mask;
    u64 restorer;
};

typedef int (*vdso_clock_gettime_fn)(int clock_id, struct timespec64 *value);

static volatile u64 signal_handler_seen;
static volatile u64 signal_handler_ra;
static volatile u64 vvar_stress_sink;

struct resolved_symbol {
    void *address;
    u64 value;
    u64 load_bias;
    u64 load_offset;
    u64 load_vaddr;
};

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

static void signal_handler(void)
{
    u64 ra;
    asm volatile("mv %0, ra" : "=r"(ra));
    signal_handler_ra = ra;
    signal_handler_seen = 1;
}

static void write_literal(const char *text)
{
    u64 length = 0;
    while (text[length]) length++;
    (void)syscall3(SYS_WRITE, 1, (long)text, (long)length);
}

static char *append_literal(char *out, const char *text)
{
    while (*text) *out++ = *text++;
    return out;
}

static char *append_hex(char *out, u64 value)
{
    static const char digits[] = "0123456789abcdef";
    *out++ = '0';
    *out++ = 'x';
    for (u64 index = 0; index < 16; index++) {
        *out++ = digits[(value >> ((15 - index) * 4)) & 0xf];
    }
    return out;
}

static char *append_hex_field(char *out, const char *name, u64 value)
{
    return append_hex(append_literal(out, name), value);
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

static int find_text_section(
    const struct elf64_ehdr *header,
    u64 *text_offset,
    u64 *text_vaddr)
{
    if (header->shoff == 0 || header->shentsize != sizeof(struct elf64_shdr)
        || header->shnum == 0 || header->shstrndx >= header->shnum) {
        return 0;
    }
    const struct elf64_shdr *sections =
        (const struct elf64_shdr *)((const char *)header + header->shoff);
    const struct elf64_shdr *strings = &sections[header->shstrndx];
    if (strings->type != 3) return 0;
    const char *names = (const char *)header + strings->offset;
    for (u16 index = 0; index < header->shnum; index++) {
        const struct elf64_shdr *section = &sections[index];
        if (section->type == 1 && string_equal(names + section->name, ".text")) {
            *text_offset = section->offset;
            *text_vaddr = section->addr;
            return 1;
        }
    }
    return 0;
}

static void write_layout_marker(
    const struct elf64_ehdr *header,
    const struct resolved_symbol *clock_gettime,
    u64 text_offset,
    u64 text_vaddr,
    u64 auxv,
    u64 auipc_address,
    i64 vvar_pc_delta)
{
    char marker[512];
    char *out = marker;
    const u64 target = (u64)(unsigned long)clock_gettime->address;
    const u64 first_instruction = clock_gettime->load_bias + text_vaddr;

    out = append_literal(out, "vdso-phase5:layout e_phoff=");
    out = append_hex(out, header->phoff);
    out = append_hex_field(out, " load-off=", clock_gettime->load_offset);
    out = append_hex_field(out, " load-vaddr=", clock_gettime->load_vaddr);
    out = append_hex_field(out, " text-off=", text_offset);
    out = append_hex_field(out, " text-vaddr=", text_vaddr);
    out = append_hex_field(out, " dynsym-st-value=", clock_gettime->value);
    out = append_hex_field(out, " load-bias=", clock_gettime->load_bias);
    out = append_hex_field(out, " resolver=", clock_gettime->load_bias + clock_gettime->value);
    out = append_hex_field(out, " jalr-target=", target);
    out = append_hex_field(out, " first-insn=", first_instruction);
    out = append_hex_field(out, " auipc=", auipc_address);
    out = append_hex_field(out, " vvar-delta=", (u64)vvar_pc_delta);
    out = append_hex_field(out, " expected-vvar=", auxv - 4096);
    out = append_hex_field(out, " actual-vvar=", (u64)((i64)auipc_address + vvar_pc_delta));
    out = append_literal(out, " target-2byte=");
    out = append_literal(out, (target & 1) == 0 ? "ok" : "bad");
    out = append_literal(out, " target-first-insn=");
    out = append_literal(out, target == first_instruction ? "ok" : "bad");
    *out++ = '\n';
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
}

static void write_signal_restorer_marker(u64 restorer, u64 captured_ra)
{
    char marker[160];
    char *out = marker;
    out = append_literal(out, "vdso-phase5:signal-restorer=pass restorer=");
    out = append_hex(out, restorer);
    out = append_literal(out, " captured-ra=");
    out = append_hex(out, captured_ra);
    out = append_literal(out, " ra=ok\n");
    (void)syscall3(SYS_WRITE, 1, (long)marker, (long)(out - marker));
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
    u64 load_offset = 0;
    u64 load_vaddr = 0;
    int have_load_bias = 0;

    for (u16 index = 0; index < header->phnum; index++) {
        if (program->type == PT_LOAD && !have_load_bias) {
            load_bias = (u64)(unsigned long)base + program->offset - program->vaddr;
            load_offset = program->offset;
            load_vaddr = program->vaddr;
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
        result.value = symbol->value;
        result.load_bias = load_bias;
        result.load_offset = load_offset;
        result.load_vaddr = load_vaddr;
        return result;
    }
    return result;
}

static int vvar_address_from_guest_code(
    const struct resolved_symbol *clock_gettime,
    u64 *auipc_address,
    i64 *vvar_pc_delta)
{
    const u32 *instructions = (const u32 *)clock_gettime->address;
    for (u64 index = 0; index < 16; index++) {
        u32 auipc = instructions[index];
        if ((auipc & 0x7f) != 0x17 || ((auipc >> 7) & 0x1f) != 8 || auipc >> 12 != 0) {
            continue;
        }
        u32 lui = instructions[index + 1];
        u32 addiw = instructions[index + 2];
        if ((lui & 0x7f) != 0x37 || ((lui >> 7) & 0x1f) != 5
            || (addiw & 0x7f) != 0x1b || ((addiw >> 7) & 0x1f) != 5
            || ((addiw >> 15) & 0x1f) != 5) {
            return 0;
        }
        *auipc_address = (u64)(unsigned long)&instructions[index];
        *vvar_pc_delta = (i64)(i32)(lui & 0xfffff000U) + ((i64)(i32)addiw >> 20);
        return 1;
    }
    return 0;
}

static int valid_timespec(const struct timespec64 *value)
{
    return value->tv_sec >= 0 && value->tv_nsec >= 0 && value->tv_nsec < 1000000000L;
}

static int mode_is(u64 *stack, const char *mode)
{
    return stack[0] > 1 && string_equal((const char *)(unsigned long)stack[2], mode);
}

static int vvar_stress_writer(void)
{
    struct timespec64 value;
    for (u64 generation = 0; generation < VVAR_STRESS_UPDATES; generation++) {
        value.tv_sec = 1800000000L + (i64)generation;
        value.tv_nsec = (i64)((generation * 1000003U) % 1000000000U);
        if (syscall3(SYS_CLOCK_SETTIME, CLOCK_REALTIME, (long)&value, 0) != 0) {
            return 0;
        }
    }
    return 1;
}

static int vvar_stress_reader(vdso_clock_gettime_fn vdso_clock_gettime)
{
    for (u64 iteration = 0; iteration < VVAR_STRESS_READS; iteration++) {
        struct timespec64 value;
        if (vdso_clock_gettime(CLOCK_REALTIME, &value) != 0 || !valid_timespec(&value)) {
            return 0;
        }
        vvar_stress_sink = (u64)value.tv_sec ^ (u64)value.tv_nsec;
    }
    return 1;
}

static int monotonic_not_before(const struct timespec64 *left, const struct timespec64 *right)
{
    return right->tv_sec > left->tv_sec
        || (right->tv_sec == left->tv_sec && right->tv_nsec >= left->tv_nsec);
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
    if (auxv == 0) {
        write_literal("vdso-phase5:fail=auxv-zero\n");
        return 1;
    }
    const u8 *image = (const u8 *)(unsigned long)auxv;
    if (image[0] != 0x7f || image[1] != 'E' || image[2] != 'L' || image[3] != 'F') {
        write_literal("vdso-phase5:fail=elf-magic\n");
        return 1;
    }
    write_literal("vdso-phase5:auxv-nonzero elf=ok\n");

    struct resolved_symbol clock_gettime_symbol = find_versioned_symbol(
        (void *)(unsigned long)auxv, "__vdso_clock_gettime", "LINUX_4.15");
    struct resolved_symbol gettimeofday_symbol = find_versioned_symbol(
        (void *)(unsigned long)auxv, "__vdso_gettimeofday", "LINUX_4.15");
    struct resolved_symbol clock_getres_symbol = find_versioned_symbol(
        (void *)(unsigned long)auxv, "__vdso_clock_getres", "LINUX_4.15");
    struct resolved_symbol rt_sigreturn_symbol = find_versioned_symbol(
        (void *)(unsigned long)auxv, "__vdso_rt_sigreturn", "LINUX_4.15");
    if (!clock_gettime_symbol.address || !gettimeofday_symbol.address || !clock_getres_symbol.address
        || !rt_sigreturn_symbol.address) {
        write_literal("vdso-phase5:fail=LINUX_4.15-symbols\n");
        return 1;
    }
    write_literal("vdso-phase5:symbols=4@LINUX_4.15\n");

    vdso_clock_gettime_fn vdso_clock_gettime = (vdso_clock_gettime_fn)clock_gettime_symbol.address;
    if (mode_is(stack, "vvar-writer")) {
        return vvar_stress_writer() ? 0 : 1;
    }
    if (mode_is(stack, "vvar-reader")) {
        return vvar_stress_reader(vdso_clock_gettime) ? 0 : 1;
    }

    u64 text_offset = 0;
    u64 text_vaddr = 0;
    if (!find_text_section((const struct elf64_ehdr *)image, &text_offset, &text_vaddr)) {
        write_literal("vdso-phase5:fail=text-section\n");
        return 1;
    }
    u64 auipc_address = 0;
    i64 vvar_pc_delta = 0;
    if (!vvar_address_from_guest_code(
            &clock_gettime_symbol,
            &auipc_address,
            &vvar_pc_delta)) {
        write_literal("vdso-phase5:fail=vvar-code-shape\n");
        return 1;
    }
    write_layout_marker(
        (const struct elf64_ehdr *)image,
        &clock_gettime_symbol,
        text_offset,
        text_vaddr,
        auxv,
        auipc_address,
        vvar_pc_delta);
    if (clock_gettime_symbol.value != text_vaddr
        || ((u64)(unsigned long)clock_gettime_symbol.address & 1)
        || ((u64)(unsigned long)clock_gettime_symbol.address
            != clock_gettime_symbol.load_bias + text_vaddr)) {
        write_literal("vdso-phase5:fail=resolved-target\n");
        return 1;
    }
    struct timespec64 realtime;
    struct timespec64 monotonic_before;
    struct timespec64 monotonic_after;
    if (vdso_clock_gettime(CLOCK_REALTIME, &realtime) != 0
        || vdso_clock_gettime(CLOCK_MONOTONIC, &monotonic_before) != 0
        || vdso_clock_gettime(CLOCK_MONOTONIC, &monotonic_after) != 0
        || !valid_timespec(&realtime)
        || !valid_timespec(&monotonic_before)
        || !valid_timespec(&monotonic_after)
        || !monotonic_not_before(&monotonic_before, &monotonic_after)) {
        write_literal("vdso-phase5:fail=supported-clock\n");
        return 1;
    }
    write_literal("vdso-phase5:supported=realtime-ok monotonic-ok\n");

    struct rt_sigaction64 signal_action = {
        .handler = (u64)(unsigned long)signal_handler,
        .flags = 0,
        .mask = 0,
        .restorer = 0,
    };
    if (syscall4(SYS_RT_SIGACTION, SIGUSR1, (long)&signal_action, 0, SIGSET_SIZE) != 0) {
        write_literal("vdso-phase5:fail=rt_sigaction\n");
        return 1;
    }
    long pid = syscall3(SYS_GETPID, 0, 0, 0);
    if (pid <= 0 || syscall3(SYS_KILL, pid, SIGUSR1, 0) != 0) {
        write_literal("vdso-phase5:fail=kill\n");
        return 1;
    }
    if (!signal_handler_seen
        || signal_handler_ra != (u64)(unsigned long)rt_sigreturn_symbol.address) {
        write_literal("vdso-phase5:fail=signal-restorer\n");
        return 1;
    }
    write_signal_restorer_marker(
        (u64)(unsigned long)rt_sigreturn_symbol.address,
        signal_handler_ra);

    struct timespec64 unsupported;
    if (vdso_clock_gettime(FALLBACK_CLOCK_ID, &unsupported) != -ENOSYS_VALUE) {
        write_literal("vdso-phase5:fail=unsupported-vdso\n");
        return 1;
    }
    if (syscall3(SYS_CLOCK_GETTIME, FALLBACK_CLOCK_ID, (long)&unsupported, 0) != -EINVAL_VALUE) {
        write_literal("vdso-phase5:fail=fallback-syscall\n");
        return 1;
    }
    write_literal("vdso-phase5:fallback=clock_gettime-syscall errno=EINVAL\n");
    write_literal("vdso-phase5:pass\n");
    return 0;
}
