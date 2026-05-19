typedef unsigned long usize;

enum {
    AT_FDCWD = -100,
    STDOUT_FILENO = 1,

    AF_UNIX = 1,
    AF_NETLINK = 16,
    SOCK_DGRAM = 2,
    SOCK_RAW = 3,
    SOCK_CLOEXEC = 0x80000,
    NETLINK_ROUTE = 0,

    CLONE_NEWNET = 0x40000000,

    SIOCGIFINDEX = 0x8933,

    NLM_F_REQUEST = 0x0001,
    NLM_F_ACK = 0x0004,
    NLMSG_ERROR = 2,
    RTM_SETLINK = 19,
    IFLA_MASTER = 10,
    IFLA_NET_NS_PID = 19,

    NR_IOCTL = 29,
    NR_OPENAT = 56,
    NR_CLOSE = 57,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_NANOSLEEP = 101,
    NR_UNSHARE = 97,
    NR_SOCKET = 198,
    NR_SENDTO = 206,
    NR_RECVFROM = 207,
    NR_EXECVE = 221,
    NR_SETNS = 268,
};

struct timespec {
    long tv_sec;
    long tv_nsec;
};

static long syscall6(long nr, long a0, long a1, long a2, long a3, long a4, long a5) {
    register long x10 asm("a0") = a0;
    register long x11 asm("a1") = a1;
    register long x12 asm("a2") = a2;
    register long x13 asm("a3") = a3;
    register long x14 asm("a4") = a4;
    register long x15 asm("a5") = a5;
    register long x17 asm("a7") = nr;
    asm volatile("ecall"
                 : "+r"(x10)
                 : "r"(x11), "r"(x12), "r"(x13), "r"(x14), "r"(x15), "r"(x17)
                 : "memory");
    return x10;
}

static void sys_exit(long code) {
    (void)syscall6(NR_EXIT, code, 0, 0, 0, 0, 0);
    for (;;) {
    }
}

static long sys_write(long fd, const void *buf, usize len) {
    return syscall6(NR_WRITE, fd, (long)buf, (long)len, 0, 0, 0);
}

static long sys_openat(long dirfd, const char *path, long flags, long mode) {
    return syscall6(NR_OPENAT, dirfd, (long)path, flags, mode, 0, 0);
}

static long sys_close(long fd) {
    return syscall6(NR_CLOSE, fd, 0, 0, 0, 0, 0);
}

static long sys_ioctl(long fd, long request, void *argp) {
    return syscall6(NR_IOCTL, fd, request, (long)argp, 0, 0, 0);
}

static long sys_socket(long domain, long type, long protocol) {
    return syscall6(NR_SOCKET, domain, type, protocol, 0, 0, 0);
}

static long sys_sendto(long fd, const void *buf, usize len, long flags,
                       const void *addr, long addrlen) {
    return syscall6(NR_SENDTO, fd, (long)buf, (long)len, flags, (long)addr, addrlen);
}

static long sys_recvfrom(long fd, void *buf, usize len, long flags,
                         void *addr, void *addrlen) {
    return syscall6(NR_RECVFROM, fd, (long)buf, (long)len, flags, (long)addr, (long)addrlen);
}

static long sys_unshare(long flags) {
    return syscall6(NR_UNSHARE, flags, 0, 0, 0, 0, 0);
}

static long sys_setns(long fd, long nstype) {
    return syscall6(NR_SETNS, fd, nstype, 0, 0, 0, 0);
}

static long sys_execve(const char *path, char **argv, char **envp) {
    return syscall6(NR_EXECVE, (long)path, (long)argv, (long)envp, 0, 0, 0);
}

static long sys_nanosleep(const struct timespec *req) {
    return syscall6(NR_NANOSLEEP, (long)req, 0, 0, 0, 0, 0);
}

static usize cstrlen(const char *s) {
    usize n = 0;
    while (s[n] != 0) {
        n++;
    }
    return n;
}

static int streq(const char *a, const char *b) {
    usize i = 0;
    for (;;) {
        if (a[i] != b[i]) {
            return 0;
        }
        if (a[i] == 0) {
            return 1;
        }
        i++;
    }
}

static void puts_lit(const char *s) {
    (void)sys_write(STDOUT_FILENO, s, cstrlen(s));
}

static void fail(void) {
    puts_lit("tx-netns-helper-fail\n");
    sys_exit(1);
}

static void fail_lit(const char *s) {
    puts_lit(s);
    sys_exit(1);
}

static void append_lit(char *out, usize *pos, const char *s) {
    while (*s != 0) {
        out[*pos] = *s;
        *pos += 1;
        s++;
    }
}

static int append_pid(char *out, usize *pos, const char *s) {
    char tmp[16];
    usize n = 0;
    if (*s == 0) {
        return 0;
    }
    while (*s != 0) {
        if (*s < '0' || *s > '9' || n >= sizeof(tmp)) {
            return 0;
        }
        tmp[n++] = *s++;
    }
    for (usize i = 0; i < n; i++) {
        out[*pos] = tmp[i];
        *pos += 1;
    }
    return 1;
}

static int parse_positive_long(const char *s, long *out) {
    long value = 0;
    if (*s == 0) {
        return 0;
    }
    while (*s != 0) {
        if (*s < '0' || *s > '9') {
            return 0;
        }
        value = value * 10 + (*s - '0');
        s++;
    }
    if (value <= 0) {
        return 0;
    }
    *out = value;
    return 1;
}

static int build_netns_path(char *out, usize out_len, const char *pid) {
    usize pos = 0;
    append_lit(out, &pos, "/proc/");
    if (!append_pid(out, &pos, pid)) {
        return 0;
    }
    append_lit(out, &pos, "/ns/net");
    if (pos + 1 > out_len) {
        return 0;
    }
    out[pos] = 0;
    return 1;
}

static void zero_bytes(char *p, usize len) {
    for (usize i = 0; i < len; i++) {
        p[i] = 0;
    }
}

static void copy_ifname(char *dst, const char *name) {
    usize i = 0;
    for (; i < 15 && name[i] != 0; i++) {
        dst[i] = name[i];
    }
    for (; i < 16; i++) {
        dst[i] = 0;
    }
}

static void put_u16(char *buf, usize off, unsigned value) {
    buf[off + 0] = (char)(value & 0xff);
    buf[off + 1] = (char)((value >> 8) & 0xff);
}

static void put_u32(char *buf, usize off, unsigned value) {
    buf[off + 0] = (char)(value & 0xff);
    buf[off + 1] = (char)((value >> 8) & 0xff);
    buf[off + 2] = (char)((value >> 16) & 0xff);
    buf[off + 3] = (char)((value >> 24) & 0xff);
}

static int get_i32(const char *buf, usize off) {
    unsigned value = ((unsigned char)buf[off + 0])
        | ((unsigned char)buf[off + 1] << 8)
        | ((unsigned char)buf[off + 2] << 16)
        | ((unsigned char)buf[off + 3] << 24);
    return (int)value;
}

static usize align4(usize value) {
    return (value + 3) & ~(usize)3;
}

static int ifindex_for(const char *name) {
    char ifreq[40];
    long fd;
    int ifindex;

    zero_bytes(ifreq, sizeof(ifreq));
    copy_ifname(ifreq, name);

    fd = sys_socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        return -1;
    }
    if (sys_ioctl(fd, SIOCGIFINDEX, ifreq) < 0) {
        (void)sys_close(fd);
        return -1;
    }
    ifindex = get_i32(ifreq, 16);
    (void)sys_close(fd);
    return ifindex;
}

static usize append_u32_attr(char *buf, usize pos, unsigned kind, unsigned value) {
    put_u16(buf, pos, 8);
    put_u16(buf, pos + 2, kind);
    put_u32(buf, pos + 4, value);
    return align4(pos + 8);
}

static int rtnl_setlink_u32_attr(int ifindex, unsigned attr_kind, unsigned attr_value) {
    char req[128];
    char resp[256];
    char sockaddr_nl[12];
    long fd;
    long sent;
    long got;
    usize pos;

    zero_bytes(req, sizeof(req));
    zero_bytes(resp, sizeof(resp));
    zero_bytes(sockaddr_nl, sizeof(sockaddr_nl));
    put_u16(sockaddr_nl, 0, AF_NETLINK);

    pos = 16 + 16;
    pos = append_u32_attr(req, pos, attr_kind, attr_value);

    put_u32(req, 0, (unsigned)pos);
    put_u16(req, 4, RTM_SETLINK);
    put_u16(req, 6, NLM_F_REQUEST | NLM_F_ACK);
    put_u32(req, 8, 0x7103);
    req[16] = 0;
    put_u16(req, 18, 0);
    put_u32(req, 20, (unsigned)ifindex);
    put_u32(req, 24, 0);
    put_u32(req, 28, 0);

    fd = sys_socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    if (fd < 0) {
        return 0;
    }
    sent = sys_sendto(fd, req, pos, 0, sockaddr_nl, sizeof(sockaddr_nl));
    if (sent < 0) {
        (void)sys_close(fd);
        return 0;
    }
    got = sys_recvfrom(fd, resp, sizeof(resp), 0, 0, 0);
    (void)sys_close(fd);
    if (got < 20) {
        return 0;
    }
    if ((unsigned)get_i32(resp, 4) != NLMSG_ERROR) {
        return 0;
    }
    return get_i32(resp, 16) == 0;
}

static void hold_mode(void) {
    static const struct timespec nap = {60, 0};
    if (sys_unshare(CLONE_NEWNET) < 0) {
        fail_lit("tx-netns-helper-unshare-fail\n");
    }
    puts_lit("tx-netns-holder-ready\n");
    for (;;) {
        (void)sys_nanosleep(&nap);
    }
}

static void exec_mode(long argc, char **argv) {
    char path[64];
    static char *empty_env[] = {0};

    if (argc < 4 || !build_netns_path(path, sizeof(path), argv[2])) {
        fail();
    }

    long fd = sys_openat(AT_FDCWD, path, 0, 0);
    if (fd < 0) {
        fail_lit("tx-netns-helper-open-fail\n");
    }
    if (sys_setns(fd, CLONE_NEWNET) < 0) {
        fail_lit("tx-netns-helper-setns-fail\n");
    }
    (void)sys_close(fd);
    (void)sys_execve(argv[3], &argv[3], empty_env);
    fail();
}

static void master_mode(long argc, char **argv) {
    int dev_ifindex;
    int master_ifindex;

    if (argc < 4) {
        fail();
    }
    dev_ifindex = ifindex_for(argv[2]);
    master_ifindex = ifindex_for(argv[3]);
    if (dev_ifindex <= 0 || master_ifindex <= 0) {
        fail_lit("tx-netns-helper-master-ifindex-fail\n");
    }
    if (!rtnl_setlink_u32_attr(dev_ifindex, IFLA_MASTER, (unsigned)master_ifindex)) {
        fail_lit("tx-netns-helper-master-fail\n");
    }
}

static void netns_mode(long argc, char **argv) {
    int dev_ifindex;
    long pid;

    if (argc < 4 || !parse_positive_long(argv[3], &pid)) {
        fail();
    }
    dev_ifindex = ifindex_for(argv[2]);
    if (dev_ifindex <= 0) {
        fail_lit("tx-netns-helper-netns-ifindex-fail\n");
    }
    if (!rtnl_setlink_u32_attr(dev_ifindex, IFLA_NET_NS_PID, (unsigned)pid)) {
        fail_lit("tx-netns-helper-netns-fail\n");
    }
}

void start_c(long *stack) {
    long argc = stack[0];
    char **argv = (char **)&stack[1];

    if (argc >= 2 && streq(argv[1], "hold")) {
        hold_mode();
    }
    if (argc >= 2 && streq(argv[1], "exec")) {
        exec_mode(argc, argv);
    }
    if (argc >= 2 && streq(argv[1], "master")) {
        master_mode(argc, argv);
        sys_exit(0);
    }
    if (argc >= 2 && streq(argv[1], "netns")) {
        netns_mode(argc, argv);
        sys_exit(0);
    }
    fail();
}

void _start(void) __attribute__((naked));
void _start(void) {
    asm volatile("mv a0, sp\n"
                 "tail start_c\n");
}
