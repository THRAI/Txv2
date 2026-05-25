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
    NR_CONNECT = 203,
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

static struct sockaddr_in loopback_addr(u16 port) {
    struct sockaddr_in addr;
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    addr.sin_addr = htonl(0x7f000001U);
    for (usize i = 0; i < sizeof(addr.sin_zero); i++) {
        addr.sin_zero[i] = 0;
    }
    return addr;
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
    static const char msg[] = "tcp-loopback-smoke-fail\n";
    (void)sys_write(STDOUT_FILENO, msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char payload[] = "tx-n68-tcp\n";
    static const char ok[] = "tx-n68-tcp-ok\n";
    char buf[32];

    struct sockaddr_in server_addr = loopback_addr(2326);
    struct sockaddr_in client_addr = loopback_addr(2327);

    long listener = sys_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (listener < 0) {
        fail();
    }
    if (sys_bind(listener, &server_addr) < 0) {
        fail();
    }
    if (sys_listen(listener, 4) < 0) {
        fail();
    }

    long client = sys_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (client < 0) {
        fail();
    }
    if (sys_bind(client, &client_addr) < 0) {
        fail();
    }
    if (sys_connect(client, &server_addr) < 0) {
        fail();
    }

    long accepted = sys_accept(listener);
    if (accepted < 0) {
        fail();
    }
    if (sys_write(client, payload, sizeof(payload) - 1) != (long)(sizeof(payload) - 1)) {
        fail();
    }
    long n = sys_read(accepted, buf, sizeof(buf));
    if (n != (long)(sizeof(payload) - 1) || !bytes_equal(buf, payload, sizeof(payload) - 1)) {
        fail();
    }

    (void)sys_close(accepted);
    (void)sys_close(client);
    (void)sys_close(listener);
    (void)sys_write(STDOUT_FILENO, ok, sizeof(ok) - 1);
    sys_exit(0);
}
