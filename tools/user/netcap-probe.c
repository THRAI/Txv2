typedef unsigned short u16;
typedef unsigned long usize;

enum {
    STDOUT_FILENO = 1,

    AF_INET = 2,
    AF_PACKET = 17,
    SOCK_RAW = 3,
    SOCK_CLOEXEC = 0x80000,
    IPPROTO_ICMP = 1,
    ETH_P_ALL = 0x0003,

    EPERM_VALUE = 1,

    NR_CLOSE = 57,
    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_SETRESUID = 147,
    NR_SOCKET = 198,
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

static long sys_setresuid(long ruid, long euid, long suid) {
    return syscall6(NR_SETRESUID, ruid, euid, suid, 0, 0, 0);
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

static long open_packet_socket(void) {
    return sys_socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, htons(ETH_P_ALL));
}

static long open_raw_icmp_socket(void) {
    return sys_socket(AF_INET, SOCK_RAW | SOCK_CLOEXEC, IPPROTO_ICMP);
}

static void close_if_open(long fd) {
    if (fd >= 0) {
        (void)sys_close(fd);
    }
}

void _start(void) {
    long packet = open_packet_socket();
    if (packet < 0) {
        fail_lit("tx-netcap-probe-root-packet-fail\n");
    }
    close_if_open(packet);

    long icmp = open_raw_icmp_socket();
    if (icmp < 0) {
        fail_lit("tx-netcap-probe-root-icmp-fail\n");
    }
    close_if_open(icmp);
    puts_lit("tx-netcap-probe-root-raw-ok\n");

    if (sys_setresuid(1000, 1000, 1000) < 0) {
        fail_lit("tx-netcap-probe-setresuid-fail\n");
    }

    packet = open_packet_socket();
    icmp = open_raw_icmp_socket();
    if (packet == -EPERM_VALUE && icmp == -EPERM_VALUE) {
        puts_lit("tx-netcap-probe-setuid-unprivileged-denied\n");
    } else if (packet >= 0 && icmp >= 0) {
        puts_lit("tx-netcap-probe-setuid-caps-preserved\n");
    } else {
        close_if_open(packet);
        close_if_open(icmp);
        fail_lit("tx-netcap-probe-setuid-mixed-result\n");
    }
    close_if_open(packet);
    close_if_open(icmp);

    puts_lit("tx-netcap-probe-success\n");
    sys_exit(0);
}
