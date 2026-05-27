typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long usize;

enum {
    STDOUT_FILENO = 1,

    AF_PACKET = 17,
    SOCK_RAW = 3,
    SOCK_CLOEXEC = 0x80000,
    ETH_P_ALL = 0x0003,

    MSG_DONTWAIT = 0x40,

    SIOCGIFINDEX = 0x8933,

    NR_IOCTL = 29,
    NR_CLOSE = 57,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_SOCKET = 198,
    NR_BIND = 200,
    NR_GETSOCKNAME = 204,
    NR_RECVFROM = 207,

    EAGAIN_VALUE = 11,
    IFNAMSIZ = 16,
};

struct ifreq_index {
    char ifr_name[IFNAMSIZ];
    int ifr_ifindex;
    char _pad[20];
};

struct sockaddr_ll {
    u16 sll_family;
    u16 sll_protocol;
    int sll_ifindex;
    u16 sll_hatype;
    unsigned char sll_pkttype;
    unsigned char sll_halen;
    unsigned char sll_addr[8];
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

static long sys_close(long fd) {
    return syscall6(NR_CLOSE, fd, 0, 0, 0, 0, 0);
}

static long sys_ioctl(long fd, long request, void *argp) {
    return syscall6(NR_IOCTL, fd, request, (long)argp, 0, 0, 0);
}

static long sys_socket(long domain, long type, long protocol) {
    return syscall6(NR_SOCKET, domain, type, protocol, 0, 0, 0);
}

static long sys_bind(long fd, const struct sockaddr_ll *addr) {
    return syscall6(NR_BIND, fd, (long)addr, sizeof(*addr), 0, 0, 0);
}

static long sys_getsockname(long fd, struct sockaddr_ll *addr, u32 *addrlen) {
    return syscall6(NR_GETSOCKNAME, fd, (long)addr, (long)addrlen, 0, 0, 0);
}

static long sys_recvfrom(long fd, void *buf, usize len, long flags) {
    return syscall6(NR_RECVFROM, fd, (long)buf, (long)len, flags, 0, 0);
}

static u16 htons(u16 value) {
    return (u16)((value << 8) | (value >> 8));
}

static usize cstrlen(const char *s) {
    usize n = 0;
    while (s[n] != 0) {
        n++;
    }
    return n;
}

static void puts_lit(const char *s) {
    (void)sys_write(STDOUT_FILENO, s, cstrlen(s));
}

static void fail_lit(const char *s) {
    puts_lit(s);
    sys_exit(1);
}

static void zero_bytes(void *ptr, usize len) {
    unsigned char *bytes = (unsigned char *)ptr;
    for (usize i = 0; i < len; i++) {
        bytes[i] = 0;
    }
}

static int copy_ifname(char out[IFNAMSIZ], const char *ifname) {
    usize i = 0;
    while (ifname[i] != 0) {
        if (i + 1 >= IFNAMSIZ) {
            return 0;
        }
        out[i] = ifname[i];
        i++;
    }
    out[i] = 0;
    return i > 0;
}

static void run_probe(const char *ifname) {
    long fd = sys_socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, htons(ETH_P_ALL));
    if (fd < 0) {
        fail_lit("tx-packet-probe-socket-fail\n");
    }

    struct ifreq_index ifreq;
    zero_bytes(&ifreq, sizeof(ifreq));
    if (!copy_ifname(ifreq.ifr_name, ifname)) {
        fail_lit("tx-packet-probe-ifname-fail\n");
    }
    if (sys_ioctl(fd, SIOCGIFINDEX, &ifreq) < 0) {
        fail_lit("tx-packet-probe-ifindex-fail\n");
    }
    if (ifreq.ifr_ifindex <= 0) {
        fail_lit("tx-packet-probe-ifindex-zero\n");
    }

    struct sockaddr_ll addr;
    zero_bytes(&addr, sizeof(addr));
    addr.sll_family = AF_PACKET;
    addr.sll_protocol = htons(ETH_P_ALL);
    addr.sll_ifindex = ifreq.ifr_ifindex;
    if (sys_bind(fd, &addr) < 0) {
        fail_lit("tx-packet-probe-bind-fail\n");
    }

    struct sockaddr_ll out;
    u32 out_len = sizeof(out);
    zero_bytes(&out, sizeof(out));
    if (sys_getsockname(fd, &out, &out_len) < 0) {
        fail_lit("tx-packet-probe-getsockname-fail\n");
    }
    if (out_len != sizeof(out)) {
        fail_lit("tx-packet-probe-getsockname-len-fail\n");
    }
    if (out.sll_family != AF_PACKET || out.sll_protocol != htons(ETH_P_ALL) ||
        out.sll_ifindex != ifreq.ifr_ifindex) {
        fail_lit("tx-packet-probe-getsockname-value-fail\n");
    }

    char buf[8];
    long recv = sys_recvfrom(fd, buf, sizeof(buf), MSG_DONTWAIT);
    if (recv != -EAGAIN_VALUE) {
        fail_lit("tx-packet-probe-recvfrom-fail\n");
    }

    (void)sys_close(fd);
    puts_lit("tx-packet-probe-ok\n");
    sys_exit(0);
}

void start_c(usize *sp) {
    long argc = (long)sp[0];
    char **argv = (char **)(&sp[1]);
    const char *ifname = "lo";
    if (argc >= 2 && argv[1] != 0) {
        ifname = argv[1];
    }
    run_probe(ifname);
}

void _start(void) __attribute__((naked));
void _start(void) {
    asm volatile("mv a0, sp\n"
                 "tail start_c\n");
}
