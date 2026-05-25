typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long usize;

enum {
    AF_INET = 2,
    SOCK_DGRAM = 2,
    IPPROTO_UDP = 17,
    STDOUT_FILENO = 1,

    NR_CLOSE = 57,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_SOCKET = 198,
    NR_BIND = 200,
    NR_SENDTO = 206,
    NR_RECVFROM = 207,
};

struct sockaddr_in {
    u16 sin_family;
    u16 sin_port;
    u32 sin_addr;
    char sin_zero[8];
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

static long sys_socket(long domain, long type, long protocol) {
    return syscall6(NR_SOCKET, domain, type, protocol, 0, 0, 0);
}

static long sys_bind(long fd, const struct sockaddr_in *addr) {
    return syscall6(NR_BIND, fd, (long)addr, sizeof(*addr), 0, 0, 0);
}

static long sys_sendto(long fd, const void *buf, usize len, const struct sockaddr_in *addr) {
    return syscall6(NR_SENDTO, fd, (long)buf, (long)len, 0, (long)addr, sizeof(*addr));
}

static long sys_recvfrom(long fd, void *buf, usize len) {
    return syscall6(NR_RECVFROM, fd, (long)buf, (long)len, 0, 0, 0);
}

static u16 htons(u16 value) {
    return (u16)((value << 8) | (value >> 8));
}

static u32 htonl(u32 value) {
    return ((value & 0x000000ffU) << 24) | ((value & 0x0000ff00U) << 8) |
           ((value & 0x00ff0000U) >> 8) | ((value & 0xff000000U) >> 24);
}

static int bytes_equal(const char *a, const char *b, usize len) {
    for (usize i = 0; i < len; i++) {
        if (a[i] != b[i]) {
            return 0;
        }
    }
    return 1;
}

static void fail(void) {
    static const char msg[] = "udp-loopback-smoke-fail\n";
    (void)sys_write(STDOUT_FILENO, msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char payload[] = "tx-n68-udp\n";
    static const char ok[] = "tx-n68-udp-ok\n";
    char buf[32];

    struct sockaddr_in addr;
    addr.sin_family = AF_INET;
    addr.sin_port = htons(2325);
    addr.sin_addr = htonl(0x7f000001U);
    for (usize i = 0; i < sizeof(addr.sin_zero); i++) {
        addr.sin_zero[i] = 0;
    }

    long fd = sys_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (fd < 0) {
        fail();
    }
    if (sys_bind(fd, &addr) < 0) {
        fail();
    }
    if (sys_sendto(fd, payload, sizeof(payload) - 1, &addr) != (long)(sizeof(payload) - 1)) {
        fail();
    }
    long n = sys_recvfrom(fd, buf, sizeof(buf));
    if (n != (long)(sizeof(payload) - 1) || !bytes_equal(buf, payload, sizeof(payload) - 1)) {
        fail();
    }

    (void)sys_close(fd);
    (void)sys_write(STDOUT_FILENO, ok, sizeof(ok) - 1);
    sys_exit(0);
}
