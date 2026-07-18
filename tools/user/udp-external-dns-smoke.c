// External UDP/DNS smoke (P2-S6 acceptance carrier). Send a hand-built DNS
// A query for example.com to the SLIRP resolver 10.0.2.3:53 and check the
// reply: matching transaction id, QR bit set, ANCOUNT >= 1. Prints
// tx-n68-dns-ok / tx-n68-dns-fail. Exercises the S6 smoltcp UDP data path
// (autobind + send_slice + process) end-to-end over the real device.

typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long usize;

enum {
    AF_INET = 2,
    SOCK_DGRAM = 2,
    IPPROTO_UDP = 17,
    STDOUT_FILENO = 1,

    NR_CLOSE = 57,
    NR_EXIT = 93,
    NR_WRITE = 64,
    NR_SOCKET = 198,
    NR_CONNECT = 203,
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
static long sys_socket(long domain, long type, long protocol) {
    return syscall6(NR_SOCKET, domain, type, protocol, 0, 0, 0);
}
static long sys_sendto(long fd, const void *buf, usize len, const struct sockaddr_in *addr) {
    return syscall6(NR_SENDTO, fd, (long)buf, (long)len, 0, (long)addr, sizeof(*addr));
}
static long sys_recvfrom(long fd, void *buf, usize len) {
    return syscall6(NR_RECVFROM, fd, (long)buf, (long)len, 0, 0, 0);
}
static long sys_close(long fd) {
    return syscall6(NR_CLOSE, fd, 0, 0, 0, 0, 0);
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
    static const char msg[] = "tx-n68-dns-fail\n";
    say(msg, sizeof(msg) - 1);
    sys_exit(1);
}

void _start(void) {
    static const char ok[] = "tx-n68-dns-ok\n";
    // DNS A query for example.com, ID 0x5455, RD set.
    static const unsigned char query[] = {
        0x54, 0x55, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        7,    'e',  'x',  'a',  'm',  'p',  'l',  'e',  3,    'c',  'o',  'm',
        0,    0x00, 0x01, 0x00, 0x01,
    };
    unsigned char reply[512];

    struct sockaddr_in resolver;
    resolver.sin_family = AF_INET;
    resolver.sin_port = htons(53);
    resolver.sin_addr = htonl(0x0a000203U); // 10.0.2.3
    for (usize i = 0; i < sizeof(resolver.sin_zero); i++) {
        resolver.sin_zero[i] = 0;
    }

    long fd = sys_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (fd < 0) {
        fail();
    }
    if (sys_sendto(fd, query, sizeof(query), &resolver) != (long)sizeof(query)) {
        fail();
    }
    {
        static const char sent[] = "tx-n68-dns-sent\n";
        say(sent, sizeof(sent) - 1);
    }

    long n = sys_recvfrom(fd, reply, sizeof(reply));
    if (n < 12) {
        fail();
    }
    // Transaction id echoed, QR bit set, at least one answer record.
    if (reply[0] != 0x54 || reply[1] != 0x55) {
        fail();
    }
    if ((reply[2] & 0x80) == 0) {
        fail();
    }
    u16 ancount = (u16)((reply[6] << 8) | reply[7]);
    if (ancount == 0) {
        fail();
    }

    (void)sys_close(fd);
    say(ok, sizeof(ok) - 1);
    sys_exit(0);
}
