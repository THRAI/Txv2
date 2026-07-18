// External bulk-TX smoke (P2-S4 acceptance carrier). connect() to the SLIRP
// gateway 10.0.2.2:9000 (host runs `nc -l 9000 | wc -c`) and write 32 KiB.
// Prints tx-n68-bulk-ok when every byte was accepted by write(), so the host
// side can independently verify the byte count. Exercises the S4 device-TX
// drain loop: a multi-MSS send queue must empty across delegate passes
// instead of one-segment-per-wake.

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

    BULK_TOTAL = 32768,
    CHUNK = 4096,
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
    static const char msg[] = "tx-n68-bulk-fail\n";
    say(msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char ok[] = "tx-n68-bulk-ok\n";
    static char chunk[CHUNK];

    for (usize i = 0; i < CHUNK; i++) {
        chunk[i] = (char)('a' + (i % 26));
    }

    struct sockaddr_in server;
    server.sin_family = AF_INET;
    server.sin_port = htons(9000);
    server.sin_addr = htonl(0x0a000202U); // 10.0.2.2
    for (usize i = 0; i < sizeof(server.sin_zero); i++) {
        server.sin_zero[i] = 0;
    }

    long fd = sys_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd < 0) {
        fail();
    }
    if (sys_connect(fd, &server) < 0) {
        fail();
    }

    usize sent = 0;
    while (sent < BULK_TOTAL) {
        usize want = BULK_TOTAL - sent;
        if (want > CHUNK) {
            want = CHUNK;
        }
        long n = sys_write(fd, chunk, want);
        if (n <= 0) {
            fail();
        }
        sent += (usize)n;
    }

    (void)sys_close(fd);
    say(ok, sizeof(ok) - 1);
    sys_exit(0);
}
