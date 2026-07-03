// External inbound TCP smoke (P2-S3 acceptance carrier). listen() on
// 0.0.0.0:7777, accept() one connection (delivered by QEMU SLIRP hostfwd,
// e.g. -netdev user,...,hostfwd=tcp::17777-:7777 + host `nc 127.0.0.1
// 17777`), read a line, echo a fixed reply back. Prints tx-n68-acc-ok when
// the accepted stream carried data both ways, tx-n68-acc-fail otherwise.
// Exercises the S3 real inbound handshake: SYN -> half-open backlog child ->
// SYN-ACK out the real device -> ACK -> accept-queue promotion.

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
    NR_BIND = 200,
    NR_LISTEN = 201,
    NR_ACCEPT = 202,
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
static long sys_bind(long fd, const struct sockaddr_in *addr) {
    return syscall6(NR_BIND, fd, (long)addr, sizeof(*addr), 0, 0, 0);
}
static long sys_listen(long fd, long backlog) {
    return syscall6(NR_LISTEN, fd, backlog, 0, 0, 0, 0);
}
static long sys_accept(long fd) {
    return syscall6(NR_ACCEPT, fd, 0, 0, 0, 0, 0);
}

static u16 htons(u16 value) {
    return (u16)((value << 8) | (value >> 8));
}

static void say(const char *s, usize n) {
    (void)sys_write(STDOUT_FILENO, s, n);
}
static void fail(void) {
    static const char msg[] = "tx-n68-acc-fail\n";
    say(msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char ready[] = "tx-n68-acc-listen\n";
    static const char reply[] = "pong-from-guest\n";
    static const char ok[] = "tx-n68-acc-ok\n";
    char buf[256];

    struct sockaddr_in local;
    local.sin_family = AF_INET;
    local.sin_port = htons(7777);
    local.sin_addr = 0; // INADDR_ANY
    for (usize i = 0; i < sizeof(local.sin_zero); i++) {
        local.sin_zero[i] = 0;
    }

    long fd = sys_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd < 0) {
        fail();
    }
    if (sys_bind(fd, &local) < 0) {
        fail();
    }
    if (sys_listen(fd, 8) < 0) {
        fail();
    }
    say(ready, sizeof(ready) - 1); // host script waits for this marker

    long conn = sys_accept(fd);
    if (conn < 0) {
        fail();
    }

    long n = sys_read(conn, buf, sizeof(buf));
    if (n <= 0) {
        fail();
    }
    if (sys_write(conn, reply, sizeof(reply) - 1) != (long)(sizeof(reply) - 1)) {
        fail();
    }

    (void)sys_close(conn);
    (void)sys_close(fd);
    say(ok, sizeof(ok) - 1);
    sys_exit(0);
}
