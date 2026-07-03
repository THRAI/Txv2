// epoll-on-socket smoke (P3-S1 acceptance carrier, audit R4a). Two phases
// against the SLIRP gateway 10.0.2.2:8000 (host runs `python3 -m
// http.server`):
//   A) connect, epoll ADD (EPOLLIN), epoll_pwait(timeout=1500ms) WITHOUT
//      sending a request — a correct kernel BLOCKS the full timeout and
//      returns 0; the pre-S1 kernel returned 0 immediately (socket wait
//      carriers were invisible to the substrate registry epoll consults).
//      Verified via CLOCK_MONOTONIC elapsed >= 1200ms.
//   B) send the GET, epoll_pwait(-1... bounded 8s) — expect exactly one
//      EPOLLIN wakeup driven by the socket readiness fire, then read and
//      verify an HTTP status line.
// Prints tx-n68-epl-ok / tx-n68-epl-fail (plus tx-n68-epl-phase-a on
// phase-A success for triage).

typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long usize;
typedef long i64;

enum {
    AF_INET = 2,
    SOCK_STREAM = 1,
    IPPROTO_TCP = 6,
    STDOUT_FILENO = 1,

    NR_EPOLL_CREATE1 = 20,
    NR_EPOLL_CTL = 21,
    NR_EPOLL_PWAIT = 22,
    NR_CLOSE = 57,
    NR_READ = 63,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_CLOCK_GETTIME = 113,
    NR_SOCKET = 198,
    NR_CONNECT = 203,

    EPOLL_CTL_ADD = 1,
    EPOLLIN = 0x1,
    CLOCK_MONOTONIC = 1,
};

struct sockaddr_in {
    u16 sin_family;
    u16 sin_port;
    u32 sin_addr;
    char sin_zero[8];
};

struct timespec {
    i64 tv_sec;
    i64 tv_nsec;
};

// riscv64 epoll_event: no packing — 4B events + 4B pad + 8B data.
struct epoll_event {
    u32 events;
    u32 pad;
    unsigned long long data;
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
static long sys_socket(long domain, long type, long protocol) {
    return syscall6(NR_SOCKET, domain, type, protocol, 0, 0, 0);
}
static long sys_connect(long fd, const struct sockaddr_in *addr) {
    return syscall6(NR_CONNECT, fd, (long)addr, sizeof(*addr), 0, 0, 0);
}
static long sys_epoll_create1(long flags) {
    return syscall6(NR_EPOLL_CREATE1, flags, 0, 0, 0, 0, 0);
}
static long sys_epoll_ctl(long epfd, long op, long fd, struct epoll_event *ev) {
    return syscall6(NR_EPOLL_CTL, epfd, op, fd, (long)ev, 0, 0);
}
static long sys_epoll_pwait(long epfd, struct epoll_event *events, long maxevents, long timeout_ms) {
    return syscall6(NR_EPOLL_PWAIT, epfd, (long)events, maxevents, timeout_ms, 0, 8);
}
static i64 monotonic_ms(void) {
    struct timespec ts;
    (void)syscall6(NR_CLOCK_GETTIME, CLOCK_MONOTONIC, (long)&ts, 0, 0, 0, 0);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
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
    static const char msg[] = "tx-n68-epl-fail\n";
    say(msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char req[] = "GET /marker HTTP/1.0\r\nHost: 10.0.2.2\r\n\r\n";
    static const char phase_a[] = "tx-n68-epl-phase-a\n";
    static const char ok[] = "tx-n68-epl-ok\n";
    char buf[256];
    struct epoll_event ev;
    struct epoll_event out[4];

    struct sockaddr_in server;
    server.sin_family = AF_INET;
    server.sin_port = htons(8000);
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

    long epfd = sys_epoll_create1(0);
    if (epfd < 0) {
        fail();
    }
    ev.events = EPOLLIN;
    ev.pad = 0;
    ev.data = (unsigned long long)fd;
    if (sys_epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) != 0) {
        fail();
    }

    // Phase A: no request sent — nothing to read. A correct kernel blocks
    // the full 1500ms; the broken one returns 0 in microseconds.
    i64 before = monotonic_ms();
    long n = sys_epoll_pwait(epfd, out, 4, 1500);
    i64 elapsed = monotonic_ms() - before;
    if (n != 0 || elapsed < 1200) {
        fail();
    }
    say(phase_a, sizeof(phase_a) - 1);

    // Phase B: send the GET; the response fires recv readiness and must
    // wake the parked epoll_pwait.
    if (sys_write(fd, req, sizeof(req) - 1) != (long)(sizeof(req) - 1)) {
        fail();
    }
    n = sys_epoll_pwait(epfd, out, 4, 8000);
    if (n != 1 || (out[0].events & EPOLLIN) == 0) {
        fail();
    }
    long got = sys_read(fd, buf, sizeof(buf));
    if (got < 5 || buf[0] != 'H' || buf[1] != 'T' || buf[2] != 'T' || buf[3] != 'P' ||
        buf[4] != '/') {
        fail();
    }

    (void)syscall6(NR_CLOSE, epfd, 0, 0, 0, 0, 0);
    (void)syscall6(NR_CLOSE, fd, 0, 0, 0, 0, 0);
    say(ok, sizeof(ok) - 1);
    sys_exit(0);
}
