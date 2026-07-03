// External sequential-connect smoke (P2-S5 acceptance carrier). Four
// back-to-back connect/GET/read/close cycles against the SLIRP gateway
// 10.0.2.2:8000 (host runs `python3 -m http.server`). Prints tx-n68-seq-ok
// only if all four succeed. Exercises the S5 ephemeral-port rotation (the
// pcap must show four distinct client ports) and per-connect distinct ISNs.

typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long usize;

enum {
    AF_INET = 2,
    SOCK_STREAM = 1,
    IPPROTO_TCP = 6,
    STDOUT_FILENO = 1,

    NR_CLOSE = 57,
    NR_READ = 63,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_SOCKET = 198,
    NR_CONNECT = 203,

    ROUNDS = 4,
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

static long sys_read(long fd, void *buf, usize len) {
    return syscall6(NR_READ, fd, (long)buf, (long)len, 0, 0, 0);
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
static long sys_connect(long fd, const struct sockaddr_in *addr) {
    return syscall6(NR_CONNECT, fd, (long)addr, sizeof(*addr), 0, 0, 0);
}

static u16 htons(u16 value) {
    return (u16)((value << 8) | (value >> 8));
}
static u32 htonl(u32 value) {
    return ((value & 0x000000ffU) << 24) | ((value & 0x0000ff00U) << 8) |
           ((value & 0x00ff0000U) >> 8) | ((value & 0xff000000U) >> 24);
}

static void say(const char *s, usize n) {
    (void)sys_write(STDOUT_FILENO, s, n);
}
static void fail(void) {
    static const char msg[] = "tx-n68-seq-fail\n";
    say(msg, sizeof(msg) - 1);
    sys_exit(1);
}

static void one_round(const struct sockaddr_in *server) {
    static const char req[] = "GET /marker HTTP/1.0\r\nHost: 10.0.2.2\r\n\r\n";
    char buf[256];

    long fd = sys_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd < 0) {
        fail();
    }
    if (sys_connect(fd, server) < 0) {
        fail();
    }
    if (sys_write(fd, req, sizeof(req) - 1) != (long)(sizeof(req) - 1)) {
        fail();
    }
    long n = sys_read(fd, buf, sizeof(buf));
    if (n < 5 || buf[0] != 'H' || buf[1] != 'T' || buf[2] != 'T' || buf[3] != 'P' ||
        buf[4] != '/') {
        fail();
    }
    (void)sys_close(fd);
}

void _start(void) {
    static const char ok[] = "tx-n68-seq-ok\n";

    struct sockaddr_in server;
    server.sin_family = AF_INET;
    server.sin_port = htons(8000);
    server.sin_addr = htonl(0x0a000202U); // 10.0.2.2
    for (usize i = 0; i < sizeof(server.sin_zero); i++) {
        server.sin_zero[i] = 0;
    }

    for (int i = 0; i < ROUNDS; i++) {
        one_round(&server);
    }

    say(ok, sizeof(ok) - 1);
    sys_exit(0);
}
