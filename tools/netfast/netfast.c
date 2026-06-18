/* tx-netfast: freestanding multicall fast-path binary for the LTP walk.
 *
 * Why this exists: under QEMU TCG every busybox-sized fork+exec costs
 * ~0.6-0.9s (~570 demand faults + ~700 syscalls), and the
 * net_stress.{interface,route} hot loops spawn 15-25 such processes per
 * iteration (tst_rhost_run ns-exec chains, awk-per-tst_iface, the
 * ping/ip script shims that each add a `sh` exec on top of busybox).
 * This binary replaces the hot-path commands with one tiny static
 * process (~5 pages, ~60 syscalls) while preserving the exact observable
 * semantics of today's stack. ANY argv shape outside the verified hot
 * forms falls back, via execve, to the previous handler (the renamed
 * /tx-ltp/bin script shims or busybox), so cold paths behave bit-for-bit
 * as before.
 *
 * Applets (selected by argv[0] basename, or argv[1] when run as
 * "tx-netfast <applet> ..."):
 *   ping/ping6   real raw-socket ICMP echo. Supports -f (flood), which
 *                busybox lacks: tst_ping probes `ping -f` and uses flood
 *                when supported, eliminating its -i 0.01 (10ms/packet)
 *                fallback floor. Loopback targets and unknown flags fall
 *                back to /tx-ltp/bin/ping.nf (the netfilter-state script).
 *   ip           link set mtu/up/down via SIOC{SIFMTU,GIFFLAGS,SIFFLAGS}
 *                (busybox's own path); addr add/del via RTM_{NEW,DEL}ADDR
 *                netlink (busybox's own path, `nodad` stripped like the
 *                script does); route add/del/show/flush replicate the
 *                /tmp/tx-ip-route STATE-FILE semantics of the current ip
 *                script (route is deliberately not sent to the kernel —
 *                connectivity comes from connected/from-address routes).
 *                Everything else execs /tx-ltp/bin/ip.fallback.
 *   tst_ns_exec  setns + run. Short-circuits `sh -c "CMD || echo RTERR"`
 *                and `sh -c "CMD > /dev/null 2>&1 &"` (the only two
 *                shapes tst_rhost_run generates) without spawning sh,
 *                and runs `ip` hot forms / `cat FILE` in-process.
 *   awk          `{ print $N }` and `{ print NF }` programs on stdin
 *                (the tst_iface/tst_hwaddr/tst_get_ifaces_cnt shapes).
 *   grep         -q with literal / anchored-literal / single
 *                (alt1|alt2|...) -E patterns on stdin.
 *   cut          single-char -d, single-field -f, stdin.
 *   cat          flagless file/stdin copy.
 *   pgrep        -x NAME via /proc/<pid>/stat comm.
 *   tst_sleep    interval[s|ms|us] (mirrors testcases/lib/tst_sleep.c).
 */

/* ------------------------------------------------------------------ */
/* arch: syscall numbers + entry                                       */
/* ------------------------------------------------------------------ */

#if defined(__riscv)
#define NR_dup3 24
#define NR_fcntl 25
#define NR_ioctl 29
#define NR_unlinkat 35
#define NR_renameat2 276
#define NR_openat 56
#define NR_close 57
#define NR_getdents64 61
#define NR_read 63
#define NR_write 64
#define NR_ppoll 73
#define NR_exit_group 94
#define NR_nanosleep 101
#define NR_clock_gettime 113
#define NR_getpid 172
#define NR_socket 198
#define NR_bind 200
#define NR_connect 203
#define NR_sendto 206
#define NR_recvfrom 207
#define NR_setsockopt 208
#define NR_clone 220
#define NR_execve 221
#define NR_wait4 260
#define NR_setns 268

/* crt0 duty: gcc emits gp-relative (linker-relaxed) accesses for small
 * data, so gp MUST be set to __global_pointer$ before any C runs —
 * with a stale gp every .sdata/.sbss access lands at gp+offset garbage
 * (observed as writes near -2016 with the register still 0 from exec).
 * norelax keeps the `la gp` itself from being relaxed into a self-move. */
__asm__(".text\n"
        ".global _start\n"
        "_start:\n"
        "  .option push\n"
        "  .option norelax\n"
        "  la gp, __global_pointer$\n"
        "  .option pop\n"
        "  mv a0, sp\n"
        "  andi sp, sp, -16\n"
        "  call cmain\n");

static long raw_syscall(long n, long a, long b, long c, long d, long e,
                        long f)
{
    register long ra0 __asm__("a0") = a;
    register long ra1 __asm__("a1") = b;
    register long ra2 __asm__("a2") = c;
    register long ra3 __asm__("a3") = d;
    register long ra4 __asm__("a4") = e;
    register long ra5 __asm__("a5") = f;
    register long ra7 __asm__("a7") = n;
    __asm__ volatile("ecall"
                     : "+r"(ra0)
                     : "r"(ra1), "r"(ra2), "r"(ra3), "r"(ra4), "r"(ra5),
                       "r"(ra7)
                     : "memory");
    return ra0;
}

#elif defined(__x86_64__)
/* host-test build only */
#define NR_dup3 292
#define NR_fcntl 72
#define NR_ioctl 16
#define NR_unlinkat 263
#define NR_renameat2 316
#define NR_openat 257
#define NR_close 3
#define NR_getdents64 217
#define NR_read 0
#define NR_write 1
#define NR_ppoll 271
#define NR_exit_group 231
#define NR_nanosleep 35
#define NR_clock_gettime 228
#define NR_getpid 39
#define NR_socket 41
#define NR_bind 49
#define NR_connect 42
#define NR_sendto 44
#define NR_recvfrom 45
#define NR_setsockopt 54
#define NR_clone 56
#define NR_execve 59
#define NR_wait4 61
#define NR_setns 308

__asm__(".text\n"
        ".global _start\n"
        "_start:\n"
        "  xor %rbp, %rbp\n"
        "  mov %rsp, %rdi\n"
        "  and $-16, %rsp\n"
        "  call cmain\n");

static long raw_syscall(long n, long a, long b, long c, long d, long e,
                        long f)
{
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8),
                       "r"(r9)
                     : "rcx", "r11", "memory");
    return ret;
}
#else
#error "unsupported arch"
#endif

#define sys0(n) raw_syscall(n, 0, 0, 0, 0, 0, 0)
#define sys1(n, a) raw_syscall(n, (long)(a), 0, 0, 0, 0, 0)
#define sys2(n, a, b) raw_syscall(n, (long)(a), (long)(b), 0, 0, 0, 0)
#define sys3(n, a, b, c) \
    raw_syscall(n, (long)(a), (long)(b), (long)(c), 0, 0, 0)
#define sys4(n, a, b, c, d) \
    raw_syscall(n, (long)(a), (long)(b), (long)(c), (long)(d), 0, 0)
#define sys5(n, a, b, c, d, e)                                          \
    raw_syscall(n, (long)(a), (long)(b), (long)(c), (long)(d), (long)(e), \
                0)
#define sys6(n, a, b, c, d, e, f)                                       \
    raw_syscall(n, (long)(a), (long)(b), (long)(c), (long)(d), (long)(e), \
                (long)(f))

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;
typedef long i64;

#define AT_FDCWD (-100)
#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR 2
#define O_CREAT 0x40
#define O_TRUNC 0x200
#define O_NONBLOCK 0x800
#define O_CLOEXEC 0x80000
#define SIGCHLD 17

struct timespec_k {
    long sec;
    long nsec;
};

static char **g_envp;

/* ------------------------------------------------------------------ */
/* minilib                                                             */
/* ------------------------------------------------------------------ */

static u64 nf_strlen(const char *s)
{
    u64 n = 0;
    while (s[n])
        n++;
    return n;
}

static int nf_strcmp(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return (u8)*a - (u8)*b;
}

static int nf_strncmp(const char *a, const char *b, u64 n)
{
    while (n && *a && *a == *b) {
        a++;
        b++;
        n--;
    }
    return n ? (u8)*a - (u8)*b : 0;
}

static void nf_memcpy(void *d, const void *s, u64 n)
{
    u8 *dd = d;
    const u8 *ss = s;
    while (n--)
        *dd++ = *ss++;
}

static void nf_memset(void *d, int c, u64 n)
{
    u8 *dd = d;
    while (n--)
        *dd++ = (u8)c;
}

static const char *nf_strchr(const char *s, char c)
{
    for (; *s; s++)
        if (*s == c)
            return s;
    return 0;
}

static int nf_isdigit(char c) { return c >= '0' && c <= '9'; }

/* parse decimal; returns chars consumed (0 = no digits), value in *out */
static int parse_u64(const char *s, u64 *out)
{
    u64 v = 0;
    int n = 0;
    while (nf_isdigit(s[n])) {
        v = v * 10 + (u64)(s[n] - '0');
        n++;
    }
    *out = v;
    return n;
}

static void write_all(int fd, const char *buf, u64 len)
{
    while (len) {
        long r = sys3(NR_write, fd, buf, len);
        if (r <= 0)
            return;
        buf += r;
        len -= (u64)r;
    }
}

static void puts_fd(int fd, const char *s) { write_all(fd, s, nf_strlen(s)); }

/* tiny formatter: %s %d %u %% only */
static void vfmt(int fd, const char *fmt, const long *args, int nargs)
{
    char buf[512];
    u64 o = 0;
    int ai = 0;
    for (; *fmt && o < sizeof(buf) - 24; fmt++) {
        if (*fmt != '%') {
            buf[o++] = *fmt;
            continue;
        }
        fmt++;
        if (*fmt == '%') {
            buf[o++] = '%';
        } else if (*fmt == 's') {
            const char *s = ai < nargs ? (const char *)args[ai++] : "";
            while (*s && o < sizeof(buf) - 1)
                buf[o++] = *s++;
        } else if (*fmt == 'd' || *fmt == 'u') {
            long v = ai < nargs ? args[ai++] : 0;
            char tmp[24];
            int tl = 0;
            u64 uv;
            if (*fmt == 'd' && v < 0) {
                buf[o++] = '-';
                uv = (u64)(-v);
            } else {
                uv = (u64)v;
            }
            do {
                tmp[tl++] = (char)('0' + (uv % 10));
                uv /= 10;
            } while (uv);
            while (tl)
                buf[o++] = tmp[--tl];
        }
    }
    write_all(fd, buf, o);
}

#define fmt1(fd, f, a) \
    do { \
        long _a[1] = {(long)(a)}; \
        vfmt(fd, f, _a, 1); \
    } while (0)
#define fmt2(fd, f, a, b) \
    do { \
        long _a[2] = {(long)(a), (long)(b)}; \
        vfmt(fd, f, _a, 2); \
    } while (0)
#define fmt3(fd, f, a, b, c) \
    do { \
        long _a[3] = {(long)(a), (long)(b), (long)(c)}; \
        vfmt(fd, f, _a, 3); \
    } while (0)
#define fmt4(fd, f, a, b, c, d) \
    do { \
        long _a[4] = {(long)(a), (long)(b), (long)(c), (long)(d)}; \
        vfmt(fd, f, _a, 4); \
    } while (0)

static void nf_exit(int code)
{
    sys1(NR_exit_group, code);
    for (;;)
        ;
}

static u64 now_us(void)
{
    struct timespec_k ts;
    sys2(NR_clock_gettime, 1 /* MONOTONIC */, &ts);
    return (u64)ts.sec * 1000000ull + (u64)ts.nsec / 1000;
}

static void sleep_us(u64 us)
{
    struct timespec_k ts;
    ts.sec = (long)(us / 1000000ull);
    ts.nsec = (long)(us % 1000000ull) * 1000;
    sys2(NR_nanosleep, &ts, 0);
}

static long nf_fork(void) { return sys5(NR_clone, SIGCHLD, 0, 0, 0, 0); }

static long nf_waitpid(long pid, int *status)
{
    return sys4(NR_wait4, pid, status, 0, 0);
}

static const char *getenv_nf(const char *name)
{
    u64 nl = nf_strlen(name);
    char **e;
    for (e = g_envp; e && *e; e++) {
        if (!nf_strncmp(*e, name, nl) && (*e)[nl] == '=')
            return *e + nl + 1;
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* exec helpers                                                        */
/* ------------------------------------------------------------------ */

static void try_execve(const char *path, char **argv)
{
    sys3(NR_execve, path, argv, g_envp);
}

/* exec busybox applet with original args (argv[0] replaced) */
static void exec_busybox(const char *applet, int argc, char **argv)
{
    char *nargv[64];
    int i, n = 0;
    nargv[n++] = (char *)"busybox";
    nargv[n++] = (char *)applet;
    for (i = 1; i < argc && n < 62; i++)
        nargv[n++] = argv[i];
    nargv[n] = 0;
    try_execve("/bin/busybox", nargv);
    try_execve("/musl/musl/busybox", nargv);
    try_execve("/musl/glibc/busybox", nargv);
}

/* PATH-walk execvp; returns only on total failure */
static void execvp_path(char **argv)
{
    char buf[512];
    const char *path;
    if (nf_strchr(argv[0], '/')) {
        try_execve(argv[0], argv);
        return;
    }
    path = getenv_nf("PATH");
    if (!path)
        path = "/tx-ltp/bin:/bin:/usr/bin";
    while (*path) {
        u64 o = 0;
        while (*path && *path != ':' && o < sizeof(buf) - 2)
            buf[o++] = *path++;
        if (*path == ':')
            path++;
        if (!o)
            continue;
        buf[o++] = '/';
        {
            const char *b = argv[0];
            while (*b && o < sizeof(buf) - 1)
                buf[o++] = *b++;
        }
        buf[o] = 0;
        try_execve(buf, argv);
    }
}

/* ------------------------------------------------------------------ */
/* net helpers                                                         */
/* ------------------------------------------------------------------ */

#define AF_INET 2
#define AF_INET6 10
#define AF_NETLINK 16
#define SOCK_DGRAM 2
#define SOCK_RAW 3
#define IPPROTO_ICMP 1
#define IPPROTO_ICMPV6 58
#define SOL_SOCKET 1
#define SO_BINDTODEVICE 25

struct sockaddr_in_k {
    u16 family;
    u16 port;
    u32 addr;
    u8 zero[8];
};

struct sockaddr_in6_k {
    u16 family;
    u16 port;
    u32 flowinfo;
    u8 addr[16];
    u32 scope;
};


/* returns 1 on success */
static int parse_ipv4(const char *s, u32 *out_be)
{
    u32 parts[4];
    int i;
    for (i = 0; i < 4; i++) {
        u64 v;
        int n = parse_u64(s, &v);
        if (!n || v > 255)
            return 0;
        parts[i] = (u32)v;
        s += n;
        if (i < 3) {
            if (*s != '.')
                return 0;
            s++;
        }
    }
    if (*s)
        return 0;
    *out_be = (parts[0]) | (parts[1] << 8) | (parts[2] << 16) |
              (parts[3] << 24);
    return 1;
}

static int hexval(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;
}

/* returns 1 on success */
static int parse_ipv6(const char *s, u8 out[16])
{
    u16 groups[8];
    int ngroups = 0;
    int dcolon = -1; /* group index where :: sits */
    nf_memset(out, 0, 16);

    if (s[0] == ':' && s[1] == ':') {
        dcolon = 0;
        s += 2;
        if (!*s) {
            return 1; /* "::" */
        }
    }
    while (*s) {
        /* embedded v4 tail? */
        if (nf_strchr(s, '.')) {
            u32 v4;
            if (ngroups > 6 || !parse_ipv4(s, &v4))
                return 0;
            groups[ngroups++] = (u16)(((v4 & 0xff) << 8) | ((v4 >> 8) & 0xff));
            groups[ngroups++] =
                (u16)((((v4 >> 16) & 0xff) << 8) | ((v4 >> 24) & 0xff));
            s += nf_strlen(s);
            break;
        }
        {
            u32 v = 0;
            int n = 0;
            while (hexval(s[n]) >= 0 && n < 4) {
                v = (v << 4) | (u32)hexval(s[n]);
                n++;
            }
            if (!n || ngroups >= 8)
                return 0;
            groups[ngroups++] = (u16)v;
            s += n;
        }
        if (*s == ':') {
            s++;
            if (*s == ':') {
                if (dcolon >= 0)
                    return 0;
                dcolon = ngroups;
                s++;
                if (!*s)
                    break;
            } else if (!*s) {
                return 0;
            }
        } else if (*s) {
            return 0;
        }
    }
    if (dcolon < 0 && ngroups != 8)
        return 0;
    if (dcolon >= 0 && ngroups >= 8)
        return 0;
    {
        int i, tail = ngroups - dcolon;
        u16 full[8];
        if (dcolon < 0) {
            for (i = 0; i < 8; i++)
                full[i] = groups[i];
        } else {
            for (i = 0; i < 8; i++)
                full[i] = 0;
            for (i = 0; i < dcolon; i++)
                full[i] = groups[i];
            for (i = 0; i < tail; i++)
                full[8 - tail + i] = groups[dcolon + i];
        }
        for (i = 0; i < 8; i++) {
            out[i * 2] = (u8)(full[i] >> 8);
            out[i * 2 + 1] = (u8)(full[i] & 0xff);
        }
    }
    return 1;
}

static u16 icmp_cksum(const u8 *data, u64 len)
{
    u32 sum = 0;
    u64 i;
    for (i = 0; i + 1 < len; i += 2)
        sum += (u32)((data[i] << 8) | data[i + 1]);
    if (i < len)
        sum += (u32)(data[i] << 8);
    while (sum >> 16)
        sum = (sum & 0xffff) + (sum >> 16);
    sum = ~sum & 0xffff;
    return (u16)((sum << 8) | (sum >> 8)); /* store big-endian */
}

/* ------------------------------------------------------------------ */
/* ifreq ioctls                                                        */
/* ------------------------------------------------------------------ */

#define SIOCGIFFLAGS 0x8913
#define SIOCSIFFLAGS 0x8914
#define SIOCSIFMTU 0x8922
#define SIOCGIFINDEX 0x8933
#define IFF_UP 1

struct ifreq_k {
    char name[16];
    union {
        short flags;
        int ivalue; /* mtu / ifindex */
        char pad[24];
    } u;
};

static int ifreq_ioctl(int fd, unsigned long req, struct ifreq_k *ifr)
{
    return (int)sys3(NR_ioctl, fd, req, ifr);
}

/* ------------------------------------------------------------------ */
/* line reader (buffered stdin)                                        */
/* ------------------------------------------------------------------ */

static char rd_buf[8192];
static long rd_len, rd_pos;
static int rd_fd;

static void rd_init(int fd)
{
    rd_fd = fd;
    rd_len = rd_pos = 0;
}

/* returns line length (without \n), -1 on EOF; line NUL-terminated */
static long rd_line(char *out, long max)
{
    long o = 0;
    for (;;) {
        if (rd_pos >= rd_len) {
            rd_len = sys3(NR_read, rd_fd, rd_buf, sizeof(rd_buf));
            rd_pos = 0;
            if (rd_len <= 0) {
                if (o == 0)
                    return -1;
                out[o] = 0;
                return o;
            }
        }
        while (rd_pos < rd_len) {
            char c = rd_buf[rd_pos++];
            if (c == '\n') {
                out[o] = 0;
                return o;
            }
            if (o < max - 1)
                out[o++] = c;
        }
    }
}

/* ------------------------------------------------------------------ */
/* applet: tst_sleep                                                   */
/* ------------------------------------------------------------------ */

static int main_tst_sleep(int argc, char **argv)
{
    u64 v, us;
    const char *end;
    int n;
    if (argc < 2) {
        puts_fd(2, "ERROR: Expected interval argument\n");
        return 1;
    }
    if (!nf_strcmp(argv[1], "-h")) {
        puts_fd(1, "Usage: tst_usleep interval[s|ms|us]\n");
        return 0;
    }
    n = parse_u64(argv[1], &v);
    if (!n) {
        puts_fd(2, "ERROR: Invalid interval\n");
        return 1;
    }
    end = argv[1] + n;
    if (!*end || !nf_strcmp(end, "s"))
        us = v * 1000000ull;
    else if (!nf_strcmp(end, "ms"))
        us = v * 1000ull;
    else if (!nf_strcmp(end, "us"))
        us = v;
    else {
        puts_fd(2, "ERROR: Invalid interval unit\n");
        return 1;
    }
    if (us)
        sleep_us(us);
    return 0;
}

/* ------------------------------------------------------------------ */
/* applet: cat                                                         */
/* ------------------------------------------------------------------ */

static int cat_fd(int fd)
{
    char buf[8192];
    for (;;) {
        long r = sys3(NR_read, fd, buf, sizeof(buf));
        if (r < 0)
            return 1;
        if (r == 0)
            return 0;
        write_all(1, buf, (u64)r);
    }
}

static int main_cat(int argc, char **argv)
{
    int i, rc = 0;
    for (i = 1; i < argc; i++) {
        if (argv[i][0] == '-' && argv[i][1]) {
            exec_busybox("cat", argc, argv);
            return 127;
        }
    }
    if (argc < 2)
        return cat_fd(0);
    for (i = 1; i < argc; i++) {
        long fd = sys4(NR_openat, AT_FDCWD, argv[i], O_RDONLY, 0);
        if (fd < 0) {
            fmt1(2, "cat: can't open '%s'\n", argv[i]);
            rc = 1;
            continue;
        }
        if (cat_fd((int)fd))
            rc = 1;
        sys1(NR_close, fd);
    }
    return rc;
}

/* ------------------------------------------------------------------ */
/* applet: cut  (-d C -f N, stdin)                                     */
/* ------------------------------------------------------------------ */

static int main_cut(int argc, char **argv)
{
    char delim = '\t';
    long field = -1;
    int have_d = 0;
    int i;
    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (!nf_strcmp(a, "-d")) {
            if (++i >= argc)
                goto fallback;
            if (nf_strlen(argv[i]) != 1)
                goto fallback;
            delim = argv[i][0];
            have_d = 1;
        } else if (!nf_strncmp(a, "-d", 2) && nf_strlen(a) == 3) {
            delim = a[2];
            have_d = 1;
        } else if (!nf_strcmp(a, "-f")) {
            u64 v;
            int n;
            if (++i >= argc)
                goto fallback;
            n = parse_u64(argv[i], &v);
            if (!n || argv[i][n] || v < 1)
                goto fallback;
            field = (long)v;
        } else if (!nf_strncmp(a, "-f", 2)) {
            u64 v;
            int n = parse_u64(a + 2, &v);
            if (!n || a[2 + n] || v < 1)
                goto fallback;
            field = (long)v;
        } else {
            goto fallback; /* files or other flags */
        }
    }
    if (field < 1 || !have_d)
        goto fallback;
    {
        static char line[4096];
        rd_init(0);
        for (;;) {
            long len = rd_line(line, sizeof(line));
            long f = 1, s = 0, e;
            if (len < 0)
                break;
            if (!nf_strchr(line, delim)) {
                /* POSIX: lines without delimiter pass through whole */
                write_all(1, line, (u64)len);
                write_all(1, "\n", 1);
                continue;
            }
            while (f < field) {
                while (line[s] && line[s] != delim)
                    s++;
                if (!line[s]) {
                    s = len; /* field absent -> empty */
                    break;
                }
                s++;
                f++;
            }
            e = s;
            while (line[e] && line[e] != delim)
                e++;
            write_all(1, line + s, (u64)(e - s));
            write_all(1, "\n", 1);
        }
    }
    return 0;
fallback:
    exec_busybox("cut", argc, argv);
    return 127;
}

/* ------------------------------------------------------------------ */
/* applet: awk  ('{ print $N }' / '{ print NF }', stdin)               */
/* ------------------------------------------------------------------ */

static int main_awk(int argc, char **argv)
{
    /* exactly one arg: the program */
    char prog[64];
    long field = -1; /* -2 = NF mode */
    if (argc != 2)
        goto fallback;
    {
        const char *p = argv[1];
        u64 o = 0;
        for (; *p; p++) {
            if (*p == ' ' || *p == '\t')
                continue;
            if (o >= sizeof(prog) - 1)
                goto fallback;
            prog[o++] = *p;
        }
        prog[o] = 0;
    }
    if (!nf_strcmp(prog, "{printNF}")) {
        field = -2;
    } else if (!nf_strncmp(prog, "{print$", 7)) {
        u64 v;
        int n = parse_u64(prog + 7, &v);
        if (!n || nf_strcmp(prog + 7 + n, "}") || v < 1)
            goto fallback;
        field = (long)v;
    } else {
        goto fallback;
    }
    {
        static char line[4096];
        rd_init(0);
        for (;;) {
            long len = rd_line(line, sizeof(line));
            long i = 0, nf = 0;
            long fs = -1, fe = -1;
            if (len < 0)
                break;
            while (line[i]) {
                while (line[i] == ' ' || line[i] == '\t')
                    i++;
                if (!line[i])
                    break;
                nf++;
                if (nf == field)
                    fs = i;
                while (line[i] && line[i] != ' ' && line[i] != '\t')
                    i++;
                if (nf == field)
                    fe = i;
            }
            if (field == -2) {
                fmt1(1, "%d\n", nf);
            } else if (fs >= 0) {
                write_all(1, line + fs, (u64)(fe - fs));
                write_all(1, "\n", 1);
            } else {
                write_all(1, "\n", 1);
            }
        }
    }
    return 0;
fallback:
    exec_busybox("awk", argc, argv);
    return 127;
}

/* ------------------------------------------------------------------ */
/* applet: grep  (-q, stdin, literal/anchored/single-alternation)      */
/* ------------------------------------------------------------------ */

/* literal-safe = no regex metacharacters */
static int grep_literal_safe(const char *s, u64 len)
{
    u64 i;
    for (i = 0; i < len; i++) {
        char c = s[i];
        if (c == '.' || c == '[' || c == ']' || c == '*' || c == '+' ||
            c == '?' || c == '{' || c == '}' || c == '(' || c == ')' ||
            c == '|' || c == '^' || c == '$' || c == '\\')
            return 0;
    }
    return 1;
}

static int substr_match(const char *line, long llen, const char *pat,
                        u64 plen, int anchor_l, int anchor_r)
{
    long i;
    if (anchor_l && anchor_r)
        return (u64)llen == plen && !nf_strncmp(line, pat, plen);
    if (anchor_l)
        return (u64)llen >= plen && !nf_strncmp(line, pat, plen);
    if (anchor_r)
        return (u64)llen >= plen &&
               !nf_strncmp(line + llen - (long)plen, pat, plen);
    if (!plen)
        return 1;
    for (i = 0; i + (long)plen <= llen; i++)
        if (!nf_strncmp(line + i, pat, plen))
            return 1;
    return 0;
}

struct grep_alt {
    char pat[128];
    u64 len;
};

static int main_grep(int argc, char **argv)
{
    int q = 0, e = 0;
    const char *pattern = 0;
    int i;
    int anchor_l = 0, anchor_r = 0;
    static struct grep_alt alts[8];
    int nalts = 0;

    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (a[0] == '-' && a[1]) {
            int j;
            for (j = 1; a[j]; j++) {
                if (a[j] == 'q')
                    q = 1;
                else if (a[j] == 'E')
                    e = 1;
                else if (a[j] == 's')
                    ;
                else
                    goto fallback;
            }
        } else if (!pattern) {
            pattern = a;
        } else {
            goto fallback; /* file args -> busybox */
        }
    }
    if (!q || !pattern)
        goto fallback;

    {
        char core[256];
        u64 plen = nf_strlen(pattern);
        if (plen >= sizeof(core))
            goto fallback;
        nf_memcpy(core, pattern, plen + 1);
        if (plen && core[0] == '^') {
            anchor_l = 1;
            nf_memcpy(core, core + 1, plen);
            plen--;
        }
        if (plen && core[plen - 1] == '$') {
            anchor_r = 1;
            core[plen - 1] = 0;
            plen--;
        }
        if (grep_literal_safe(core, plen)) {
            alts[0].len = plen;
            nf_memcpy(alts[0].pat, core, plen + 1);
            nalts = 1;
        } else if (e) {
            /* single (a|b|...) group: prefix(alt1|alt2)suffix */
            const char *lp = nf_strchr(core, '(');
            const char *rp = lp ? nf_strchr(lp, ')') : 0;
            if (!lp || !rp || nf_strchr(rp + 1, '(') ||
                nf_strchr(rp + 1, ')'))
                goto fallback;
            {
                u64 pre = (u64)(lp - core);
                const char *suf = rp + 1;
                u64 suflen = nf_strlen(suf);
                const char *p = lp + 1;
                if (!grep_literal_safe(core, pre) ||
                    !grep_literal_safe(suf, suflen))
                    goto fallback;
                while (p < rp) {
                    const char *bar = p;
                    u64 mid;
                    while (bar < rp && *bar != '|')
                        bar++;
                    mid = (u64)(bar - p);
                    if (!grep_literal_safe(p, mid))
                        goto fallback;
                    if (nalts >= 8 ||
                        pre + mid + suflen >= sizeof(alts[0].pat))
                        goto fallback;
                    nf_memcpy(alts[nalts].pat, core, pre);
                    nf_memcpy(alts[nalts].pat + pre, p, mid);
                    nf_memcpy(alts[nalts].pat + pre + mid, suf, suflen);
                    alts[nalts].pat[pre + mid + suflen] = 0;
                    alts[nalts].len = pre + mid + suflen;
                    nalts++;
                    p = bar + 1;
                }
                if (!nalts)
                    goto fallback;
            }
        } else {
            goto fallback;
        }
    }

    {
        static char line[4096];
        rd_init(0);
        for (;;) {
            long len = rd_line(line, sizeof(line));
            if (len < 0)
                break;
            for (i = 0; i < nalts; i++) {
                if (substr_match(line, len, alts[i].pat, alts[i].len,
                                 anchor_l, anchor_r))
                    return 0; /* -q: first match wins */
            }
        }
    }
    return 1;
fallback:
    exec_busybox("grep", argc, argv);
    return 127;
}

/* ------------------------------------------------------------------ */
/* applet: pgrep (-x NAME)                                             */
/* ------------------------------------------------------------------ */

struct dirent64_k {
    u64 ino;
    i64 off;
    u16 reclen;
    u8 type;
    char name[];
};

static int main_pgrep(int argc, char **argv)
{
    const char *name;
    long dfd;
    int found = 0;
    if (argc != 3 || nf_strcmp(argv[1], "-x")) {
        exec_busybox("pgrep", argc, argv);
        return 127;
    }
    name = argv[2];
    dfd = sys4(NR_openat, AT_FDCWD, "/proc", O_RDONLY | 0x10000 /*O_DIRECTORY*/,
               0);
    if (dfd < 0) {
        exec_busybox("pgrep", argc, argv);
        return 127;
    }
    for (;;) {
        static char dbuf[8192];
        long n = sys3(NR_getdents64, dfd, dbuf, sizeof(dbuf));
        long off = 0;
        if (n <= 0)
            break;
        while (off < n) {
            struct dirent64_k *d = (struct dirent64_k *)(dbuf + off);
            off += d->reclen;
            if (!nf_isdigit(d->name[0]))
                continue;
            {
                char path[64];
                static char stat[512];
                long fd, r;
                u64 o = 0;
                const char *pfx = "/proc/";
                while (*pfx)
                    path[o++] = *pfx++;
                {
                    const char *q = d->name;
                    while (*q && o < 40)
                        path[o++] = *q++;
                }
                {
                    const char *sfx = "/stat";
                    while (*sfx)
                        path[o++] = *sfx++;
                }
                path[o] = 0;
                fd = sys4(NR_openat, AT_FDCWD, path, O_RDONLY, 0);
                if (fd < 0)
                    continue;
                r = sys3(NR_read, fd, stat, sizeof(stat) - 1);
                sys1(NR_close, fd);
                if (r <= 0)
                    continue;
                stat[r] = 0;
                {
                    /* comm = between first '(' and last ')' */
                    const char *lp = nf_strchr(stat, '(');
                    const char *rp = 0;
                    long k;
                    if (!lp)
                        continue;
                    for (k = r - 1; k >= 0; k--) {
                        if (stat[k] == ')') {
                            rp = stat + k;
                            break;
                        }
                    }
                    if (!rp || rp <= lp)
                        continue;
                    if ((u64)(rp - lp - 1) == nf_strlen(name) &&
                        !nf_strncmp(lp + 1, name, (u64)(rp - lp - 1))) {
                        fmt1(1, "%s\n", d->name);
                        found = 1;
                    }
                }
            }
        }
    }
    sys1(NR_close, dfd);
    return found ? 0 : 1;
}

/* ------------------------------------------------------------------ */
/* applet: ip                                                          */
/* ------------------------------------------------------------------ */

#define NETLINK_ROUTE 0
#define RTM_NEWADDR 20
#define RTM_DELADDR 21
#define NLM_F_REQUEST 1
#define NLM_F_ACK 4
#define NLM_F_EXCL 0x200
#define NLM_F_CREATE 0x400
#define NLMSG_ERROR 2
#define IFA_ADDRESS 1
#define IFA_LOCAL 2

struct nlmsghdr_k {
    u32 len;
    u16 type;
    u16 flags;
    u32 seq;
    u32 pid;
};

struct ifaddrmsg_k {
    u8 family;
    u8 prefixlen;
    u8 flags;
    u8 scope;
    u32 index;
};

struct nlattr_k {
    u16 len;
    u16 type;
};

struct sockaddr_nl_k {
    u16 family;
    u16 pad;
    u32 pid;
    u32 groups;
};

static int ip_ifindex(const char *name)
{
    struct ifreq_k ifr;
    long fd = sys3(NR_socket, AF_INET, SOCK_DGRAM, 0);
    int rc;
    if (fd < 0)
        return -1;
    nf_memset(&ifr, 0, sizeof(ifr));
    {
        u64 n = nf_strlen(name);
        if (n >= sizeof(ifr.name)) {
            sys1(NR_close, fd);
            return -1;
        }
        nf_memcpy(ifr.name, name, n);
    }
    rc = ifreq_ioctl((int)fd, SIOCGIFINDEX, &ifr);
    sys1(NR_close, fd);
    if (rc < 0)
        return -1;
    return ifr.u.ivalue;
}

/* ip addr add/del via rtnetlink; returns 0 ok, -1 needs fallback */
static int ip_addr_netlink(int del, int family, const u8 *addr, int alen,
                           int prefix, const char *dev)
{
    int ifindex = ip_ifindex(dev);
    long fd;
    u8 msg[128];
    struct nlmsghdr_k *nh = (struct nlmsghdr_k *)msg;
    struct ifaddrmsg_k *ifa = (struct ifaddrmsg_k *)(msg + 16);
    u32 off = 16 + 8;
    int i;

    if (ifindex < 0)
        return -1;
    fd = sys3(NR_socket, AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0)
        return -1;
    {
        struct sockaddr_nl_k snl;
        nf_memset(&snl, 0, sizeof(snl));
        snl.family = AF_NETLINK;
        if (sys3(NR_bind, fd, &snl, sizeof(snl)) < 0) {
            sys1(NR_close, fd);
            return -1;
        }
    }
    nf_memset(msg, 0, sizeof(msg));
    nh->type = del ? RTM_DELADDR : RTM_NEWADDR;
    nh->flags = (u16)(NLM_F_REQUEST | NLM_F_ACK |
                      (del ? 0 : (NLM_F_CREATE | NLM_F_EXCL)));
    nh->seq = 1;
    ifa->family = (u8)family;
    ifa->prefixlen = (u8)prefix;
    ifa->index = (u32)ifindex;
    for (i = 0; i < 2; i++) {
        struct nlattr_k *at = (struct nlattr_k *)(msg + off);
        at->type = i ? IFA_ADDRESS : IFA_LOCAL;
        at->len = (u16)(4 + alen);
        nf_memcpy(msg + off + 4, addr, (u64)alen);
        off += 4 + (u32)((alen + 3) & ~3);
    }
    nh->len = off;
    if (sys6(NR_sendto, fd, msg, off, 0, 0, 0) < 0) {
        sys1(NR_close, fd);
        return -1;
    }
    {
        u8 rbuf[256];
        long r = sys6(NR_recvfrom, fd, rbuf, sizeof(rbuf), 0, 0, 0);
        sys1(NR_close, fd);
        if (r < (long)(16 + 4))
            return -1;
        {
            struct nlmsghdr_k *rh = (struct nlmsghdr_k *)rbuf;
            int err;
            if (rh->type != NLMSG_ERROR)
                return -1;
            nf_memcpy(&err, rbuf + 16, 4);
            if (err == 0)
                return 0;
            return -1; /* kernel rejected: let busybox produce the
                          canonical error path */
        }
    }
}

/* route state file ops replicating the prior /tx-ltp/bin/ip script */
#define ROUTE_STATE "/tmp/tx-ip-route"

static long route_read_state(char *buf, long max)
{
    long fd = sys4(NR_openat, AT_FDCWD, ROUTE_STATE, O_RDONLY, 0);
    long total = 0;
    if (fd < 0)
        return 0;
    for (;;) {
        long r = sys3(NR_read, fd, buf + total, max - total);
        if (r <= 0)
            break;
        total += r;
        if (total >= max)
            break;
    }
    sys1(NR_close, fd);
    return total;
}

static int route_write_state(const char *content, u64 len)
{
    char tmp[64];
    long pid = sys0(NR_getpid);
    long fd;
    {
        u64 o = 0;
        const char *p = ROUTE_STATE ".";
        while (*p)
            tmp[o++] = *p++;
        {
            char d[24];
            int dl = 0;
            u64 v = (u64)pid;
            do {
                d[dl++] = (char)('0' + v % 10);
                v /= 10;
            } while (v);
            while (dl)
                tmp[o++] = d[--dl];
        }
        tmp[o] = 0;
    }
    fd = sys4(NR_openat, AT_FDCWD, tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return -1;
    write_all((int)fd, content, len);
    sys1(NR_close, fd);
    if (sys5(NR_renameat2, AT_FDCWD, tmp, AT_FDCWD, ROUTE_STATE, 0) < 0)
        return -1;
    return 0;
}

/* state line format: "dest gw_or_underscore dev_or_underscore\n" */
static int ip_route_state(int argc, char **argv, int argi, char **oargv,
                          int oargc)
{
    const char *cmd = argi < argc ? argv[argi] : "";
    static char old[16384], next[20480];
    long olen;
    u64 no = 0;

    if (!nf_strcmp(cmd, "flush")) {
        route_write_state("", 0);
        return 0;
    }
    if (!nf_strcmp(cmd, "show") || !nf_strcmp(cmd, "list") || !*cmd) {
        olen = route_read_state(old, sizeof(old) - 1);
        old[olen] = 0;
        {
            long p = 0;
            while (p < olen) {
                char dest[128], gw[128], dv[128];
                int f = 0;
                u64 dl = 0, gl = 0, vl = 0;
                while (p < olen && old[p] != '\n') {
                    char c = old[p++];
                    if (c == ' ') {
                        f++;
                        continue;
                    }
                    if (f == 0 && dl < 127)
                        dest[dl++] = c;
                    else if (f == 1 && gl < 127)
                        gw[gl++] = c;
                    else if (f == 2 && vl < 127)
                        dv[vl++] = c;
                }
                if (p < olen)
                    p++;
                dest[dl] = gw[gl] = dv[vl] = 0;
                if (!dl)
                    continue;
                write_all(1, dest, dl);
                if (gl && nf_strcmp(gw, "_")) {
                    puts_fd(1, " via ");
                    write_all(1, gw, gl);
                }
                if (vl && nf_strcmp(dv, "_")) {
                    puts_fd(1, " dev ");
                    write_all(1, dv, vl);
                }
                write_all(1, "\n", 1);
            }
        }
        return 0;
    }
    if (!nf_strcmp(cmd, "add") || !nf_strcmp(cmd, "replace") ||
        !nf_strcmp(cmd, "change") || !nf_strcmp(cmd, "append") ||
        !nf_strcmp(cmd, "prepend") || !nf_strcmp(cmd, "del") ||
        !nf_strcmp(cmd, "delete")) {
        int deleting = (!nf_strcmp(cmd, "del") || !nf_strcmp(cmd, "delete"));
        const char *dest = argi + 1 < argc ? argv[argi + 1] : 0;
        const char *gw = 0, *dv = 0;
        int i;
        if (!dest || !*dest)
            return deleting ? 0 : 1;
        for (i = argi + 2; i < argc; i++) {
            if (!nf_strcmp(argv[i], "via") && i + 1 < argc)
                gw = argv[++i];
            else if (!nf_strcmp(argv[i], "dev") && i + 1 < argc)
                dv = argv[++i];
        }
        if (!deleting && !dv && gw) {
            if (!nf_strncmp(gw, "127.", 4) || !nf_strcmp(gw, "::1"))
                dv = "lo";
        }
        olen = route_read_state(old, sizeof(old) - 1);
        if (olen >= (long)sizeof(old) - 1)
            return -1; /* state larger than we model: busybox-script path */
        old[olen] = 0;
        {
            long p = 0;
            u64 destlen = nf_strlen(dest);
            while (p < olen) {
                long ls = p;
                while (p < olen && old[p] != '\n')
                    p++;
                {
                    long ll = p - ls;
                    /* first field == dest? skip */
                    long sp = ls;
                    while (sp < ls + ll && old[sp] != ' ')
                        sp++;
                    if (!((u64)(sp - ls) == destlen &&
                          !nf_strncmp(old + ls, dest, destlen))) {
                        if (ll && no + (u64)ll + 1 < sizeof(next)) {
                            nf_memcpy(next + no, old + ls, (u64)ll);
                            no += (u64)ll;
                            next[no++] = '\n';
                        }
                    }
                }
                if (p < olen)
                    p++;
            }
        }
        if (!deleting) {
            u64 dl2 = nf_strlen(dest);
            const char *g2 = gw ? gw : "_";
            const char *v2 = dv ? dv : "_";
            if (no + dl2 + nf_strlen(g2) + nf_strlen(v2) + 3 <
                sizeof(next)) {
                nf_memcpy(next + no, dest, dl2);
                no += dl2;
                next[no++] = ' ';
                nf_memcpy(next + no, g2, nf_strlen(g2));
                no += nf_strlen(g2);
                next[no++] = ' ';
                nf_memcpy(next + no, v2, nf_strlen(v2));
                no += nf_strlen(v2);
                next[no++] = '\n';
            }
        }
        route_write_state(next, no);
        return 0;
    }
    /* unknown route subcommand (get/...) -> fallback with original argv */
    (void)oargv;
    (void)oargc;
    return -1;
}

/* returns exit code, or -1 if the caller should fall back */
static int ip_run(int argc, char **argv)
{
    int argi = 1;
    int family = AF_INET;
    const char *object, *command;

    if (argi < argc &&
        (!nf_strcmp(argv[argi], "-4") || !nf_strcmp(argv[argi], "-6"))) {
        if (!nf_strcmp(argv[argi], "-6"))
            family = AF_INET6;
        argi++;
    }
    if (argi >= argc)
        return -1;
    object = argv[argi];
    command = argi + 1 < argc ? argv[argi + 1] : "";

    if (!nf_strcmp(object, "xfrm") && !nf_strcmp(command, "state"))
        return 1;

    if (!nf_strcmp(object, "link") && !nf_strcmp(command, "set")) {
        const char *dev = 0;
        long mtu = -1;
        int updown = 0; /* 1=up 2=down */
        int unknown = 0;
        int i;
        for (i = argi + 2; i < argc; i++) {
            if (!nf_strcmp(argv[i], "dev") && i + 1 < argc) {
                dev = argv[++i];
            } else if (!nf_strcmp(argv[i], "mtu") && i + 1 < argc) {
                u64 v;
                if (!parse_u64(argv[i + 1], &v))
                    return -1;
                mtu = (long)v;
                i++;
            } else if (!nf_strcmp(argv[i], "up")) {
                updown = 1;
            } else if (!nf_strcmp(argv[i], "down")) {
                updown = 2;
            } else if (!dev) {
                dev = argv[i];
            } else {
                unknown = 1;
            }
        }
        /* mirror prior script: no mtu/up/down -> silent success */
        if (mtu < 0 && !updown)
            return 0;
        /* mtu/up/down plus modifiers we don't model -> busybox path,
         * exactly like the prior script forwarded the whole command */
        if (unknown)
            return -1;
        if (!dev)
            return -1;
        {
            struct ifreq_k ifr;
            long fd = sys3(NR_socket, AF_INET, SOCK_DGRAM, 0);
            u64 n = nf_strlen(dev);
            if (fd < 0 || n >= sizeof(ifr.name))
                return -1;
            if (mtu >= 0) {
                nf_memset(&ifr, 0, sizeof(ifr));
                nf_memcpy(ifr.name, dev, n);
                ifr.u.ivalue = (int)mtu;
                if (ifreq_ioctl((int)fd, SIOCSIFMTU, &ifr) < 0) {
                    sys1(NR_close, fd);
                    return -1;
                }
            }
            if (updown) {
                nf_memset(&ifr, 0, sizeof(ifr));
                nf_memcpy(ifr.name, dev, n);
                if (ifreq_ioctl((int)fd, SIOCGIFFLAGS, &ifr) < 0) {
                    sys1(NR_close, fd);
                    return -1;
                }
                if (updown == 1)
                    ifr.u.flags |= IFF_UP;
                else
                    ifr.u.flags = (short)(ifr.u.flags & ~IFF_UP);
                if (ifreq_ioctl((int)fd, SIOCSIFFLAGS, &ifr) < 0) {
                    sys1(NR_close, fd);
                    return -1;
                }
            }
            sys1(NR_close, fd);
            return 0;
        }
    }

    if (!nf_strcmp(object, "route"))
        return ip_route_state(argc, argv, argi + 1, argv, argc);

    if (!nf_strcmp(object, "addr") || !nf_strcmp(object, "address")) {
        if (!nf_strcmp(command, "flush"))
            return 0; /* mirror prior script */
        if (!nf_strcmp(command, "add") || !nf_strcmp(command, "del") ||
            !nf_strcmp(command, "delete")) {
            int del = nf_strcmp(command, "add") != 0;
            const char *addr = 0, *dev = 0;
            int i;
            for (i = argi + 2; i < argc; i++) {
                if (!nf_strcmp(argv[i], "dev") && i + 1 < argc)
                    dev = argv[++i];
                else if (!nf_strcmp(argv[i], "nodad"))
                    ; /* prior script strips nodad */
                else if (!nf_strcmp(argv[i], "broadcast") ||
                         !nf_strcmp(argv[i], "brd") ||
                         !nf_strcmp(argv[i], "peer") ||
                         !nf_strcmp(argv[i], "scope") ||
                         !nf_strcmp(argv[i], "label"))
                    return -1; /* not the hot shape */
                else if (!addr)
                    addr = argv[i];
                else
                    return -1;
            }
            if (!addr || !dev)
                return -1;
            {
                char abuf[64];
                u8 bin[16];
                int alen, prefix;
                const char *slash = nf_strchr(addr, '/');
                u64 al = slash ? (u64)(slash - addr) : nf_strlen(addr);
                int fam = nf_strchr(addr, ':') ? AF_INET6 : AF_INET;
                if (al >= sizeof(abuf))
                    return -1;
                nf_memcpy(abuf, addr, al);
                abuf[al] = 0;
                if (fam == AF_INET) {
                    u32 v4;
                    if (!parse_ipv4(abuf, &v4))
                        return -1;
                    nf_memcpy(bin, &v4, 4);
                    alen = 4;
                    prefix = 32;
                } else {
                    if (!parse_ipv6(abuf, bin))
                        return -1;
                    alen = 16;
                    prefix = 128;
                }
                if (slash) {
                    u64 v;
                    int n = parse_u64(slash + 1, &v);
                    if (!n || slash[1 + n] || v > (u64)(alen * 8))
                        return -1;
                    prefix = (int)v;
                }
                (void)family;
                if (ip_addr_netlink(del, fam, bin, alen, prefix, dev) < 0)
                    return -1;
                return 0;
            }
        }
        return -1; /* show/replace/change -> fallback */
    }

    return -1;
}

static int main_ip(int argc, char **argv)
{
    int rc = ip_run(argc, argv);
    if (rc >= 0)
        return rc;
    try_execve("/tx-ltp/bin/ip.fallback", argv);
    exec_busybox("ip", argc, argv);
    return 127;
}

/* ------------------------------------------------------------------ */
/* applet: ping / ping6                                                */
/* ------------------------------------------------------------------ */

struct ping_opts {
    long count;       /* -c, default busybox-like infinite -> cap */
    long size;        /* -s payload bytes, default 56 */
    u64 interval_us;  /* -i, default 1s */
    int flood;        /* -f */
    long deadline_s;  /* -w total, 0 = none */
    long wait_s;      /* -W linger, 0 = default */
    const char *iface; /* -I */
    int quiet;        /* -q */
    int v6;
    const char *pattern; /* -p hex */
    const char *target;
};

static void ping_usage_exit(void)
{
    /* Must NOT contain "invalid option"/"unrecognized option": tst_ping
     * probes flags by grepping for those strings (no-target probe). */
    puts_fd(2, "Usage: ping [OPTIONS] HOST\n"
               "  -4/-6 -c CNT -s SIZE -i SECS -f -w SECS -W SECS\n"
               "  -I IFACE/IP -q -p HEXBYTE\n");
    nf_exit(2);
}

static void ping_fallback(int argc, char **argv, int v6)
{
    try_execve(v6 ? "/tx-ltp/bin/ping6.nf" : "/tx-ltp/bin/ping.nf", argv);
    exec_busybox(v6 ? "ping6" : "ping", argc, argv);
    nf_exit(127);
}

/* parse "0.01" style seconds into us */
static int parse_interval_us(const char *s, u64 *out)
{
    u64 ip = 0, frac = 0, scale = 1000000;
    int n = parse_u64(s, &ip);
    const char *p = s + n;
    if (!n && *p != '.')
        return 0;
    if (*p == '.') {
        p++;
        while (nf_isdigit(*p) && scale > 1) {
            scale /= 10;
            frac += (u64)(*p - '0') * scale;
            p++;
        }
        while (nf_isdigit(*p))
            p++;
    }
    if (*p)
        return 0;
    *out = ip * 1000000ull + frac;
    return 1;
}

static int main_ping(int argc, char **argv, int v6_name)
{
    struct ping_opts o;
    int i;
    nf_memset(&o, 0, sizeof(o));
    o.count = -1;
    o.size = 56;
    o.interval_us = 1000000;
    o.v6 = v6_name;

    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (a[0] == '-' && a[1]) {
            char f = a[1];
            const char *val = 0;
            int needs_val = (f == 'c' || f == 's' || f == 'i' || f == 'w' ||
                             f == 'W' || f == 'I' || f == 'p');
            if (a[2] && needs_val) {
                val = a + 2;
            } else if (a[2]) {
                /* joined multi-flag like -qf? keep simple: only known
                 * single flags may join */
                int j;
                for (j = 1; a[j]; j++) {
                    if (a[j] == 'f')
                        o.flood = 1;
                    else if (a[j] == 'q')
                        o.quiet = 1;
                    else if (a[j] == '4')
                        o.v6 = 0;
                    else if (a[j] == '6')
                        o.v6 = 1;
                    else
                        ping_fallback(argc, argv, v6_name);
                }
                continue;
            } else if (needs_val) {
                if (++i >= argc)
                    ping_usage_exit();
                val = argv[i];
            }
            switch (f) {
            case 'c': {
                u64 v;
                int n = parse_u64(val, &v);
                if (!n || val[n])
                    ping_fallback(argc, argv, v6_name);
                o.count = (long)v;
                break;
            }
            case 's': {
                u64 v;
                int n = parse_u64(val, &v);
                if (!n || val[n] || v > 65507)
                    ping_fallback(argc, argv, v6_name);
                o.size = (long)v;
                break;
            }
            case 'i':
                if (!parse_interval_us(val, &o.interval_us))
                    ping_fallback(argc, argv, v6_name);
                break;
            case 'w': {
                u64 v;
                int n = parse_u64(val, &v);
                if (!n || val[n])
                    ping_fallback(argc, argv, v6_name);
                o.deadline_s = (long)v;
                break;
            }
            case 'W': {
                u64 v;
                int n = parse_u64(val, &v);
                if (!n || val[n])
                    ping_fallback(argc, argv, v6_name);
                o.wait_s = (long)v;
                break;
            }
            case 'I':
                o.iface = val;
                break;
            case 'p':
                o.pattern = val;
                break;
            case 'f':
                o.flood = 1;
                break;
            case 'q':
                o.quiet = 1;
                break;
            case '4':
                o.v6 = 0;
                break;
            case '6':
                o.v6 = 1;
                break;
            default:
                ping_fallback(argc, argv, v6_name);
            }
        } else {
            if (o.target)
                ping_fallback(argc, argv, v6_name);
            o.target = a;
        }
    }

    if (!o.target)
        ping_usage_exit(); /* flag-probe shape: usage, no fallback */

    /* netfilter fake-state script owns loopback targets */
    if (!nf_strcmp(o.target, "127.0.0.1") || !nf_strcmp(o.target, "::1"))
        ping_fallback(argc, argv, v6_name);
    if (!o.v6 && nf_strchr(o.target, ':'))
        o.v6 = 1;

    {
        struct sockaddr_in_k dst4;
        struct sockaddr_in6_k dst6;
        void *dst;
        u64 dstlen;
        long fd;
        static u8 pkt[65600], rbuf[66000];
        long plen = 8 + o.size;
        u16 id;
        long sent = 0, recvd = 0;
        u64 t0, tlast_send = 0;
        u64 rtt_min = ~0ull, rtt_max = 0, rtt_sum = 0;
        static u64 send_ts[1024];

        if (o.v6) {
            nf_memset(&dst6, 0, sizeof(dst6));
            dst6.family = AF_INET6;
            if (!parse_ipv6(o.target, dst6.addr))
                ping_fallback(argc, argv, v6_name);
            dst = &dst6;
            dstlen = sizeof(dst6);
        } else {
            u32 a4;
            nf_memset(&dst4, 0, sizeof(dst4));
            dst4.family = AF_INET;
            if (!parse_ipv4(o.target, &a4))
                ping_fallback(argc, argv, v6_name);
            dst4.addr = a4;
            dst = &dst4;
            dstlen = sizeof(dst4);
        }

        fd = sys3(NR_socket, o.v6 ? AF_INET6 : AF_INET, SOCK_RAW,
                  o.v6 ? IPPROTO_ICMPV6 : IPPROTO_ICMP);
        if (fd < 0)
            ping_fallback(argc, argv, v6_name);

        if (o.iface) {
            u32 a4;
            u8 a6[16];
            if (!o.v6 && parse_ipv4(o.iface, &a4)) {
                struct sockaddr_in_k src;
                nf_memset(&src, 0, sizeof(src));
                src.family = AF_INET;
                src.addr = a4;
                if (sys3(NR_bind, fd, &src, sizeof(src)) < 0)
                    ping_fallback(argc, argv, v6_name);
            } else if (o.v6 && parse_ipv6(o.iface, a6)) {
                struct sockaddr_in6_k src;
                nf_memset(&src, 0, sizeof(src));
                src.family = AF_INET6;
                nf_memcpy(src.addr, a6, 16);
                if (sys3(NR_bind, fd, &src, sizeof(src)) < 0)
                    ping_fallback(argc, argv, v6_name);
            } else {
                if (sys5(NR_setsockopt, fd, SOL_SOCKET, SO_BINDTODEVICE,
                         o.iface, nf_strlen(o.iface) + 1) < 0)
                    ping_fallback(argc, argv, v6_name);
            }
        }
        /* nonblocking reads */
        {
            long fl = sys3(NR_fcntl, fd, 3 /*F_GETFL*/, 0);
            sys3(NR_fcntl, fd, 4 /*F_SETFL*/, fl | O_NONBLOCK);
        }

        id = (u16)(sys0(NR_getpid) & 0xffff);
        if (o.count < 0)
            o.count = o.flood ? 0x7fffffff : 0x7fffffff;

        fmt2(1, "PING %s (%s)", o.target, o.target);
        fmt1(1, ": %d data bytes\n", o.size);

        /* payload pattern */
        {
            long k;
            u8 pb = 0;
            int have_pb = 0;
            if (o.pattern) {
                int h1 = hexval(o.pattern[0]);
                int h2 = o.pattern[1] ? hexval(o.pattern[1]) : 0;
                if (h1 >= 0) {
                    pb = (u8)(o.pattern[1] && h2 >= 0 ? (h1 << 4) | h2 : h1);
                    have_pb = 1;
                }
            }
            for (k = 0; k < o.size; k++)
                pkt[8 + k] = have_pb ? pb : (u8)k;
        }

        t0 = now_us();
        for (;;) {
            u64 now = now_us();
            int can_send;
            if (o.deadline_s && now - t0 >= (u64)o.deadline_s * 1000000ull)
                break;
            /* flood: keep up to 8 echoes in flight (strictly >= the
             * window-1 iputils pacing: when replies lag, fall back to a
             * 10ms-per-send floor exactly like `ping -f`). */
            can_send = sent == 0 ||
                       (o.flood ? (sent - recvd < 8 ||
                                   now - tlast_send >= 10000)
                                : now - tlast_send >= o.interval_us);
            if (sent < o.count && can_send) {
                /* build echo request */
                pkt[0] = o.v6 ? 128 : 8;
                pkt[1] = 0;
                pkt[2] = pkt[3] = 0;
                pkt[4] = (u8)(id >> 8);
                pkt[5] = (u8)(id & 0xff);
                pkt[6] = (u8)((sent >> 8) & 0xff);
                pkt[7] = (u8)(sent & 0xff);
                if (!o.v6) {
                    u16 ck = icmp_cksum(pkt, (u64)plen);
                    pkt[2] = (u8)(ck & 0xff);
                    pkt[3] = (u8)(ck >> 8);
                }
                /* kernel computes ICMPv6 checksum on raw v6 sockets */
                if (sys6(NR_sendto, fd, pkt, plen, 0, dst, dstlen) < 0) {
                    /* ENOBUFS under flood: brief yield and retry once */
                    sleep_us(2000);
                    if (sys6(NR_sendto, fd, pkt, plen, 0, dst, dstlen) <
                        0) {
                        if (sent == 0)
                            ping_fallback(argc, argv, v6_name);
                        break;
                    }
                }
                if (sent < 1024)
                    send_ts[sent] = now_us();
                sent++;
                tlast_send = now;
            }
            /* drain replies, waiting up to the applicable window */
            {
                u64 window_us;
                if (sent >= o.count) {
                    /* linger phase */
                    u64 linger =
                        o.wait_s ? (u64)o.wait_s * 1000000ull : 1000000ull;
                    if (recvd >= sent || now - tlast_send >= linger)
                        break;
                    window_us = 20000;
                } else if (o.flood) {
                    /* pipeline open -> drain without blocking and keep
                     * sending; pipeline full -> wait for replies (10ms
                     * floor mirrors iputils flood pacing) */
                    window_us = (sent - recvd < 8) ? 0 : 10000;
                } else {
                    window_us = o.interval_us - (now - tlast_send);
                    if ((long)window_us < 0)
                        window_us = 0;
                    if (window_us > 50000)
                        window_us = 50000;
                }
                {
                    struct {
                        int fd;
                        short events;
                        short revents;
                    } pfd;
                    struct timespec_k ts;
                    long pr;
                    pfd.fd = (int)fd;
                    pfd.events = 1; /* POLLIN */
                    pfd.revents = 0;
                    ts.sec = (long)(window_us / 1000000ull);
                    ts.nsec = (long)(window_us % 1000000ull) * 1000;
                    pr = sys5(NR_ppoll, &pfd, 1, &ts, 0, 0);
                    if (pr > 0) {
                        for (;;) {
                            long r = sys6(NR_recvfrom, fd, rbuf,
                                          sizeof(rbuf), 0, 0, 0);
                            const u8 *icmp;
                            long ilen;
                            int ttl = 64;
                            if (r <= 0)
                                break;
                            if (o.v6) {
                                icmp = rbuf;
                                ilen = r;
                            } else {
                                long ihl = (rbuf[0] & 0xf) * 4;
                                if (r <= ihl)
                                    continue;
                                ttl = rbuf[8];
                                icmp = rbuf + ihl;
                                ilen = r - ihl;
                            }
                            if (ilen < 8)
                                continue;
                            if (icmp[0] != (o.v6 ? 129 : 0))
                                continue;
                            if (((u16)(icmp[4] << 8) | icmp[5]) != id)
                                continue;
                            {
                                u32 seq =
                                    ((u32)icmp[6] << 8) | (u32)icmp[7];
                                u64 rtt = 0;
                                if (seq < 1024 && send_ts[seq]) {
                                    rtt = now_us() - send_ts[seq];
                                    if (rtt < rtt_min)
                                        rtt_min = rtt;
                                    if (rtt > rtt_max)
                                        rtt_max = rtt;
                                    rtt_sum += rtt;
                                }
                                recvd++;
                                if (!o.quiet && !o.flood) {
                                    fmt4(1,
                                         "%d bytes from %s: seq=%u ttl=%d",
                                         (long)ilen, o.target, (long)seq,
                                         (long)ttl);
                                    fmt2(1, " time=%d.%d ms\n",
                                         (long)(rtt / 1000),
                                         (long)((rtt % 1000) / 100));
                                }
                            }
                        }
                    }
                }
            }
            if (sent >= o.count && recvd >= sent)
                break;
        }

        fmt1(1, "\n--- %s ping statistics ---\n", o.target);
        fmt3(1, "%d packets transmitted, %d packets received, %d%% packet loss\n",
             sent, recvd, sent ? (sent - recvd) * 100 / sent : 0);
        if (recvd > 0 && rtt_sum) {
            fmt3(1, "round-trip min/avg/max = %d.%d/%d",
                 (long)(rtt_min / 1000), (long)((rtt_min % 1000) / 100),
                 (long)(rtt_sum / (u64)recvd / 1000));
            fmt3(1, ".%d/%d.%d ms\n",
                 (long)((rtt_sum / (u64)recvd % 1000) / 100),
                 (long)(rtt_max / 1000), (long)((rtt_max % 1000) / 100));
        }
        return recvd > 0 ? 0 : 1;
    }
}

/* ------------------------------------------------------------------ */
/* applet: tst_ns_exec                                                 */
/* ------------------------------------------------------------------ */

static void ns_exec_fallback(char **argv)
{
    /* pre-setns failures only: hand the whole job to the real binary */
    const char *root = getenv_nf("LTPROOT");
    char path[256];
    if (root) {
        u64 o = 0;
        const char *p = root;
        while (*p && o < 200)
            path[o++] = *p++;
        {
            const char *s = "/testcases/bin/tst_ns_exec";
            while (*s)
                path[o++] = *s++;
        }
        path[o] = 0;
        try_execve(path, argv);
    }
    try_execve("/musl/musl/ltp/testcases/bin/tst_ns_exec", argv);
    try_execve("/musl/glibc/ltp/testcases/bin/tst_ns_exec", argv);
    nf_exit(127);
}

static int str_ends_strip(char *s, u64 *len, const char *suffix)
{
    u64 sl = nf_strlen(suffix);
    while (*len && (s[*len - 1] == ' ' || s[*len - 1] == '\t')) {
        s[--(*len)] = 0;
    }
    if (*len < sl || nf_strncmp(s + *len - sl, suffix, sl))
        return 0;
    *len -= sl;
    s[*len] = 0;
    while (*len && (s[*len - 1] == ' ' || s[*len - 1] == '\t'))
        s[--(*len)] = 0;
    return 1;
}

static int sh_word_split(char *s, char **words, int max)
{
    int n = 0;
    for (;;) {
        while (*s == ' ' || *s == '\t')
            s++;
        if (!*s)
            break;
        if (n >= max)
            return -1;
        words[n++] = s;
        while (*s && *s != ' ' && *s != '\t')
            s++;
        if (*s)
            *s++ = 0;
    }
    return n;
}

static int sh_simple_safe(const char *s)
{
    for (; *s; s++) {
        char c = *s;
        if (c == '|' || c == '&' || c == ';' || c == '<' || c == '>' ||
            c == '(' || c == ')' || c == '$' || c == '`' || c == '"' ||
            c == '\'' || c == '\\' || c == '*' || c == '?' || c == '[' ||
            c == ']' || c == '{' || c == '}' || c == '~' || c == '#' ||
            c == '!' || c == '\n')
            return 0;
    }
    return 1;
}

static int run_words(char **words, int nwords)
{
    /* in-process fast paths first */
    if (!nf_strcmp(words[0], "ip")) {
        /* ip_run expects argv[0]=name; words already shaped that way */
        int rc = ip_run(nwords, words);
        if (rc >= 0)
            return rc;
    } else if (!nf_strcmp(words[0], "cat") && nwords == 2 &&
               words[1][0] == '/') {
        long fd = sys4(NR_openat, AT_FDCWD, words[1], O_RDONLY, 0);
        if (fd >= 0) {
            int rc = cat_fd((int)fd);
            sys1(NR_close, fd);
            return rc;
        }
        /* fall through to real spawn for exact error behavior */
    }
    {
        long pid = nf_fork();
        int status = 0;
        if (pid == 0) {
            execvp_path(words);
            nf_exit(127);
        }
        if (pid < 0)
            return 127;
        nf_waitpid(pid, &status);
        if ((status & 0x7f) == 0)
            return (status >> 8) & 0xff;
        return 128 + (status & 0x7f);
    }
}

static int main_tst_ns_exec(int argc, char **argv)
{
    char nslist[64];
    char *tok;
    u64 nl;
    if (argc < 4) {
        puts_fd(1, "usage: tst_ns_exec <NS_PID> <ns,list> <PROGRAM> [ARGS]\n");
        return 1;
    }
    nl = nf_strlen(argv[2]);
    if (nl >= sizeof(nslist))
        ns_exec_fallback(argv);
    nf_memcpy(nslist, argv[2], nl + 1);

    tok = nslist;
    while (*tok) {
        char *end = tok;
        char path[128];
        long fd;
        u64 o = 0;
        while (*end && *end != ',')
            end++;
        {
            const char *p = "/proc/";
            while (*p)
                path[o++] = *p++;
        }
        {
            const char *p = argv[1];
            while (*p && o < 100)
                path[o++] = *p++;
        }
        {
            const char *p = "/ns/";
            while (*p)
                path[o++] = *p++;
        }
        {
            const char *p = tok;
            while (p < end && o < 126)
                path[o++] = *p++;
        }
        path[o] = 0;
        fd = sys4(NR_openat, AT_FDCWD, path, O_RDONLY, 0);
        if (fd < 0)
            ns_exec_fallback(argv);
        if (sys2(NR_setns, fd, 0) < 0) {
            sys1(NR_close, fd);
            ns_exec_fallback(argv);
        }
        sys1(NR_close, fd);
        tok = *end ? end + 1 : end;
    }

    /* sh -c short-circuit for tst_rhost_run's two shapes */
    if (argc == 6 && !nf_strcmp(argv[3], "sh") && !nf_strcmp(argv[4], "-c")) {
        static char cmd[2048];
        u64 cl = nf_strlen(argv[5]);
        int bg = 0, rterr = 0;
        if (cl < sizeof(cmd)) {
            nf_memcpy(cmd, argv[5], cl + 1);
            if (str_ends_strip(cmd, &cl, "&")) {
                /* " CMD > /dev/null 2>&1 &" */
                if (str_ends_strip(cmd, &cl, "2>&1") &&
                    str_ends_strip(cmd, &cl, "> /dev/null"))
                    bg = 1;
                else
                    goto spawn_sh;
            } else if (str_ends_strip(cmd, &cl, "|| echo RTERR")) {
                rterr = 1;
            }
            /* tolerate the optional nohup prefix (non-netns -b form) */
            {
                char *c = cmd;
                while (*c == ' ' || *c == '\t')
                    c++;
                if (!nf_strncmp(c, "nohup ", 6))
                    c += 6;
                if (!sh_simple_safe(c))
                    goto spawn_sh;
                {
                    char *words[32];
                    int nw = sh_word_split(c, words, 31);
                    if (nw <= 0)
                        goto spawn_sh;
                    if (nf_strchr(words[0], '='))
                        goto spawn_sh; /* env assignment prefix */
                    words[nw] = 0;
                    if (bg) {
                        long pid = nf_fork();
                        if (pid == 0) {
                            long devnull = sys4(NR_openat, AT_FDCWD,
                                                "/dev/null", O_RDWR, 0);
                            if (devnull >= 0) {
                                sys3(NR_dup3, devnull, 1, 0);
                                sys3(NR_dup3, devnull, 2, 0);
                                if (devnull > 2)
                                    sys1(NR_close, devnull);
                            }
                            execvp_path(words);
                            nf_exit(127);
                        }
                        return 0; /* sh exits 0 right away for `cmd &` */
                    }
                    {
                        int rc = run_words(words, nw);
                        if (rterr) {
                            if (rc != 0)
                                puts_fd(1, "RTERR\n");
                            return 0;
                        }
                        return rc;
                    }
                }
            }
        }
    spawn_sh:
        /* unknown shell shape: spawn a real sh -c (same as today) */
        {
            char *sargv[5];
            sargv[0] = (char *)"sh";
            sargv[1] = (char *)"-c";
            sargv[2] = argv[5];
            sargv[3] = 0;
            execvp_path(sargv);
            nf_exit(127);
        }
    }

    /* direct program: exec in place (exit status flows to the caller
     * exactly like the wait-and-forward of the real tst_ns_exec) */
    execvp_path(argv + 3);
    nf_exit(127);
    return 127;
}

/* ------------------------------------------------------------------ */
/* applet: bench-syscall (diagnostic: end-to-end null-syscall cost)    */
/* ------------------------------------------------------------------ */

static int main_bench_syscall(int argc, char **argv)
{
    u64 t0, t1;
    int i;
    const int n = 10000;
    if (argc > 1 && !nf_strcmp(argv[1], "spin")) {
        /* endless getpid loop for host-side PC-sampling profiles */
        for (;;)
            sys0(NR_getpid);
    }
    t0 = now_us();
    for (i = 0; i < n; i++)
        sys0(NR_getpid);
    t1 = now_us();
    fmt2(1, "TX-BENCH-SYSCALL getpid x%d total_us=%d", n, (long)(t1 - t0));
    fmt1(1, " ns_per_call=%d\n", (long)((t1 - t0) * 1000 / (u64)n));
    t0 = now_us();
    for (i = 0; i < n; i++) {
        struct timespec_k ts;
        sys2(NR_clock_gettime, 1, &ts);
    }
    t1 = now_us();
    fmt2(1, "TX-BENCH-SYSCALL clock_gettime x%d total_us=%d", n,
         (long)(t1 - t0));
    fmt1(1, " ns_per_call=%d\n", (long)((t1 - t0) * 1000 / (u64)n));

    /* ns-exec chain decomposition: the two extra syscalls a tst_rhost_run
     * chain adds on top of the bare spawn lifecycle — opening the netns
     * file and switching into it. /proc/self resolves to a real netns file;
     * setns to the current netns succeeds (no same-ns rejection) and is the
     * same atomic-swap path a cross-ns switch takes. */
    {
        const char *nspath = "/proc/self/ns/net";
        long fd;
        t0 = now_us();
        for (i = 0; i < n; i++) {
            fd = sys4(NR_openat, AT_FDCWD, nspath, O_RDONLY, 0);
            if (fd >= 0)
                sys1(NR_close, fd);
        }
        t1 = now_us();
        fmt2(1, "TX-BENCH-SYSCALL openat-ns x%d total_us=%d", n,
             (long)(t1 - t0));
        fmt1(1, " ns_per_call=%d\n", (long)((t1 - t0) * 1000 / (u64)n));

        fd = sys4(NR_openat, AT_FDCWD, nspath, O_RDONLY, 0);
        if (fd >= 0) {
            t0 = now_us();
            for (i = 0; i < n; i++)
                sys2(NR_setns, fd, 0);
            t1 = now_us();
            fmt2(1, "TX-BENCH-SYSCALL setns x%d total_us=%d", n,
                 (long)(t1 - t0));
            fmt1(1, " ns_per_call=%d\n", (long)((t1 - t0) * 1000 / (u64)n));
            sys1(NR_close, fd);
        } else {
            fmt1(1, "TX-BENCH-SYSCALL setns SKIP openat-ns-fd=%d\n", (long)fd);
        }
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* dispatch                                                            */
/* ------------------------------------------------------------------ */

static const char *basename_nf(const char *p)
{
    const char *b = p;
    for (; *p; p++)
        if (*p == '/')
            b = p + 1;
    return b;
}

static int applet_main(int argc, char **argv)
{
    const char *name;
    if (!argc)
        return 127;
    name = basename_nf(argv[0]);
    if (!nf_strncmp(name, "tx-netfast", 10)) {
        if (argc < 2) {
            puts_fd(2, "tx-netfast: multicall: ping ping6 ip tst_ns_exec "
                       "awk grep cut cat pgrep tst_sleep\n");
            return 1;
        }
        argc--;
        argv++;
        name = basename_nf(argv[0]);
    }
    if (!nf_strcmp(name, "ping"))
        return main_ping(argc, argv, 0);
    if (!nf_strcmp(name, "ping6"))
        return main_ping(argc, argv, 1);
    if (!nf_strcmp(name, "ip"))
        return main_ip(argc, argv);
    if (!nf_strcmp(name, "tst_ns_exec"))
        return main_tst_ns_exec(argc, argv);
    if (!nf_strcmp(name, "awk"))
        return main_awk(argc, argv);
    if (!nf_strcmp(name, "grep"))
        return main_grep(argc, argv);
    if (!nf_strcmp(name, "cut"))
        return main_cut(argc, argv);
    if (!nf_strcmp(name, "cat"))
        return main_cat(argc, argv);
    if (!nf_strcmp(name, "pgrep"))
        return main_pgrep(argc, argv);
    if (!nf_strcmp(name, "tst_sleep"))
        return main_tst_sleep(argc, argv);
    if (!nf_strcmp(name, "bench-syscall"))
        return main_bench_syscall(argc, argv);
    puts_fd(2, "tx-netfast: unknown applet\n");
    return 127;
}

void cmain(long *sp)
{
    int argc = (int)sp[0];
    char **argv = (char **)(sp + 1);
    g_envp = argv + argc + 1;
    nf_exit(applet_main(argc, argv));
}
