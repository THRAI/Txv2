typedef unsigned long usize;

enum {
    AF_NETLINK = 16,
    SOCK_RAW = 3,
    SOCK_NONBLOCK = 0x800,
    SOCK_CLOEXEC = 0x80000,
    NETLINK_NETFILTER = 12,

    MSG_DONTWAIT = 0x40,

    NR_WRITE = 64,
    NR_EXIT = 93,
    NR_SOCKET = 198,
    NR_SENDTO = 206,
    NR_RECVFROM = 207,

    EAGAIN_VALUE = 11,

    NLM_F_REQUEST = 0x0001,
    NLM_F_ACK = 0x0004,
    NLM_F_CREATE = 0x0400,
    NLM_F_APPEND = 0x0800,

    NLMSG_ERROR = 2,
    NLMSG_DONE = 3,
    NLMSG_MIN_TYPE = 0x10,

    NFNL_SUBSYS_NFTABLES = 10,
    NFNL_MSG_BATCH_BEGIN = NLMSG_MIN_TYPE,
    NFNL_MSG_BATCH_END = NLMSG_MIN_TYPE + 1,

    NFT_MSG_NEWTABLE = 0,
    NFT_MSG_GETTABLE = 1,
    NFT_MSG_DELTABLE = 2,
    NFT_MSG_NEWCHAIN = 3,
    NFT_MSG_GETCHAIN = 4,
    NFT_MSG_DELCHAIN = 5,
    NFT_MSG_NEWRULE = 6,
    NFT_MSG_GETRULE = 7,
    NFT_MSG_DELRULE = 8,

    NFPROTO_IPV4 = 2,

    NLA_F_NESTED = 0x8000,

    NFTA_TABLE_NAME = 1,
    NFTA_CHAIN_TABLE = 1,
    NFTA_CHAIN_NAME = 3,
    NFTA_CHAIN_HOOK = 4,
    NFTA_CHAIN_TYPE = 7,
    NFTA_HOOK_HOOKNUM = 1,
    NFTA_HOOK_PRIORITY = 2,
    NFTA_RULE_TABLE = 1,
    NFTA_RULE_CHAIN = 2,
    NFTA_RULE_HANDLE = 3,
    NFTA_RULE_EXPRESSIONS = 4,
    NFTA_EXPR_NAME = 1,
    NFTA_EXPR_DATA = 2,
    NFTA_META_DREG = 1,
    NFTA_META_KEY = 2,
    NFTA_PAYLOAD_DREG = 1,
    NFTA_PAYLOAD_BASE = 2,
    NFTA_PAYLOAD_OFFSET = 3,
    NFTA_PAYLOAD_LEN = 4,
    NFTA_BITWISE_SREG = 1,
    NFTA_BITWISE_DREG = 2,
    NFTA_BITWISE_LEN = 3,
    NFTA_BITWISE_MASK = 4,
    NFTA_BITWISE_XOR = 5,
    NFTA_CMP_SREG = 1,
    NFTA_CMP_OP = 2,
    NFTA_CMP_DATA = 3,
    NFTA_DATA_VALUE = 1,

    NFT_REG32_00 = 8,
    NFT_CMP_EQ = 0,
    NFT_PAYLOAD_NETWORK_HEADER = 1,
    NFT_META_OIFNAME = 7,
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

static long sys_sendto(long fd, const void *buf, usize len, long flags,
                       const void *addr, long addrlen) {
    return syscall6(NR_SENDTO, fd, (long)buf, (long)len, flags, (long)addr, addrlen);
}

static long sys_recvfrom(long fd, void *buf, usize len, long flags,
                         void *addr, void *addrlen) {
    return syscall6(NR_RECVFROM, fd, (long)buf, (long)len, flags, (long)addr, (long)addrlen);
}

static usize cstrlen(const char *s) {
    usize n = 0;
    while (s[n] != 0) {
        n++;
    }
    return n;
}

static void puts_lit(const char *s) {
    (void)sys_write(1, s, cstrlen(s));
}

static void fail_lit(const char *s) {
    puts_lit(s);
    sys_exit(1);
}

static void put_u16(unsigned char *buf, usize off, unsigned value) {
    buf[off + 0] = (unsigned char)(value & 0xff);
    buf[off + 1] = (unsigned char)((value >> 8) & 0xff);
}

static void put_u32(unsigned char *buf, usize off, unsigned value) {
    buf[off + 0] = (unsigned char)(value & 0xff);
    buf[off + 1] = (unsigned char)((value >> 8) & 0xff);
    buf[off + 2] = (unsigned char)((value >> 16) & 0xff);
    buf[off + 3] = (unsigned char)((value >> 24) & 0xff);
}

static void put_u64(unsigned char *buf, usize off, unsigned long value) {
    for (usize i = 0; i < 8; i++) {
        buf[off + i] = (unsigned char)((value >> (i * 8)) & 0xff);
    }
}

static int get_u16(const unsigned char *buf, usize off) {
    return (int)((unsigned)buf[off] | ((unsigned)buf[off + 1] << 8));
}

static int get_i32(const unsigned char *buf, usize off) {
    unsigned value = ((unsigned)buf[off + 0])
        | ((unsigned)buf[off + 1] << 8)
        | ((unsigned)buf[off + 2] << 16)
        | ((unsigned)buf[off + 3] << 24);
    return (int)value;
}

static void pad4(unsigned char *buf, usize *pos) {
    while ((*pos & 3) != 0) {
        buf[*pos] = 0;
        *pos += 1;
    }
}

static usize attr_start(unsigned char *buf, usize *pos, unsigned kind) {
    usize start = *pos;
    put_u16(buf, start, 0);
    put_u16(buf, start + 2, kind);
    *pos += 4;
    return start;
}

static void attr_end(unsigned char *buf, usize *pos, usize start) {
    put_u16(buf, start, (unsigned)(*pos - start));
    pad4(buf, pos);
}

static void push_attr(unsigned char *buf, usize *pos, unsigned kind,
                      const unsigned char *payload, usize len) {
    usize start = attr_start(buf, pos, kind);
    for (usize i = 0; i < len; i++) {
        buf[*pos + i] = payload[i];
    }
    *pos += len;
    attr_end(buf, pos, start);
}

static void push_attr_string(unsigned char *buf, usize *pos, unsigned kind, const char *value) {
    usize start = attr_start(buf, pos, kind);
    for (usize i = 0; value[i] != 0; i++) {
        buf[*pos] = (unsigned char)value[i];
        *pos += 1;
    }
    buf[*pos] = 0;
    *pos += 1;
    attr_end(buf, pos, start);
}

static void push_attr_u32(unsigned char *buf, usize *pos, unsigned kind, unsigned value) {
    unsigned char tmp[4];
    put_u32(tmp, 0, value);
    push_attr(buf, pos, kind, tmp, 4);
}

static void push_attr_u64(unsigned char *buf, usize *pos, unsigned kind, unsigned long value) {
    unsigned char tmp[8];
    put_u64(tmp, 0, value);
    push_attr(buf, pos, kind, tmp, 8);
}

static void push_nfgenmsg(unsigned char *buf, usize *pos) {
    buf[*pos + 0] = NFPROTO_IPV4;
    buf[*pos + 1] = 0;
    buf[*pos + 2] = 0;
    buf[*pos + 3] = 0;
    *pos += 4;
}

static usize nlmsg_start(unsigned char *buf, usize *pos, unsigned kind,
                         unsigned flags, unsigned seq) {
    usize start = *pos;
    put_u32(buf, start, 0);
    put_u16(buf, start + 4, kind);
    put_u16(buf, start + 6, flags);
    put_u32(buf, start + 8, seq);
    put_u32(buf, start + 12, 0);
    *pos += 16;
    return start;
}

static void nlmsg_end(unsigned char *buf, usize *pos, usize start) {
    put_u32(buf, start, (unsigned)(*pos - start));
    pad4(buf, pos);
}

static unsigned nft_msg(unsigned op) {
    return (NFNL_SUBSYS_NFTABLES << 8) | op;
}

static void append_batch_marker(unsigned char *buf, usize *pos, unsigned kind, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, kind, NLM_F_REQUEST, seq);
    push_nfgenmsg(buf, pos);
    nlmsg_end(buf, pos, msg);
}

static void append_newtable(unsigned char *buf, usize *pos, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_NEWTABLE),
                            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_TABLE_NAME, "nat");
    nlmsg_end(buf, pos, msg);
}

static void append_newchain(unsigned char *buf, usize *pos, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_NEWCHAIN),
                            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_CHAIN_TABLE, "nat");
    push_attr_string(buf, pos, NFTA_CHAIN_NAME, "postrouting");
    push_attr_string(buf, pos, NFTA_CHAIN_TYPE, "nat");
    usize hook = attr_start(buf, pos, NFTA_CHAIN_HOOK | NLA_F_NESTED);
    push_attr_u32(buf, pos, NFTA_HOOK_HOOKNUM, 4);
    push_attr_u32(buf, pos, NFTA_HOOK_PRIORITY, 100);
    attr_end(buf, pos, hook);
    nlmsg_end(buf, pos, msg);
}

static void push_expr_empty(unsigned char *buf, usize *pos, unsigned index, const char *name) {
    usize expr = attr_start(buf, pos, index | NLA_F_NESTED);
    push_attr_string(buf, pos, NFTA_EXPR_NAME, name);
    attr_end(buf, pos, expr);
}

static void push_expr_meta(unsigned char *buf, usize *pos, unsigned index,
                           unsigned dreg, unsigned key) {
    usize expr = attr_start(buf, pos, index | NLA_F_NESTED);
    push_attr_string(buf, pos, NFTA_EXPR_NAME, "meta");
    usize data = attr_start(buf, pos, NFTA_EXPR_DATA | NLA_F_NESTED);
    push_attr_u32(buf, pos, NFTA_META_DREG, dreg);
    push_attr_u32(buf, pos, NFTA_META_KEY, key);
    attr_end(buf, pos, data);
    attr_end(buf, pos, expr);
}

static void push_expr_payload(unsigned char *buf, usize *pos, unsigned index,
                              unsigned dreg, unsigned base, unsigned offset, unsigned len) {
    usize expr = attr_start(buf, pos, index | NLA_F_NESTED);
    push_attr_string(buf, pos, NFTA_EXPR_NAME, "payload");
    usize data = attr_start(buf, pos, NFTA_EXPR_DATA | NLA_F_NESTED);
    push_attr_u32(buf, pos, NFTA_PAYLOAD_DREG, dreg);
    push_attr_u32(buf, pos, NFTA_PAYLOAD_BASE, base);
    push_attr_u32(buf, pos, NFTA_PAYLOAD_OFFSET, offset);
    push_attr_u32(buf, pos, NFTA_PAYLOAD_LEN, len);
    attr_end(buf, pos, data);
    attr_end(buf, pos, expr);
}

static void push_data_value(unsigned char *buf, usize *pos,
                            const unsigned char *value, usize len) {
    push_attr(buf, pos, NFTA_DATA_VALUE, value, len);
}

static void push_expr_cmp(unsigned char *buf, usize *pos, unsigned index,
                          unsigned sreg, const unsigned char *value, usize len) {
    usize expr = attr_start(buf, pos, index | NLA_F_NESTED);
    push_attr_string(buf, pos, NFTA_EXPR_NAME, "cmp");
    usize data = attr_start(buf, pos, NFTA_EXPR_DATA | NLA_F_NESTED);
    push_attr_u32(buf, pos, NFTA_CMP_SREG, sreg);
    push_attr_u32(buf, pos, NFTA_CMP_OP, NFT_CMP_EQ);
    usize cmp_data = attr_start(buf, pos, NFTA_CMP_DATA | NLA_F_NESTED);
    push_data_value(buf, pos, value, len);
    attr_end(buf, pos, cmp_data);
    attr_end(buf, pos, data);
    attr_end(buf, pos, expr);
}

static void push_expr_bitwise_mask(unsigned char *buf, usize *pos, unsigned index,
                                   unsigned sreg, unsigned dreg) {
    static const unsigned char mask[4] = {255, 255, 0, 0};
    static const unsigned char zero[4] = {0, 0, 0, 0};
    usize expr = attr_start(buf, pos, index | NLA_F_NESTED);
    push_attr_string(buf, pos, NFTA_EXPR_NAME, "bitwise");
    usize data = attr_start(buf, pos, NFTA_EXPR_DATA | NLA_F_NESTED);
    push_attr_u32(buf, pos, NFTA_BITWISE_SREG, sreg);
    push_attr_u32(buf, pos, NFTA_BITWISE_DREG, dreg);
    push_attr_u32(buf, pos, NFTA_BITWISE_LEN, 4);
    usize mask_attr = attr_start(buf, pos, NFTA_BITWISE_MASK | NLA_F_NESTED);
    push_data_value(buf, pos, mask, sizeof(mask));
    attr_end(buf, pos, mask_attr);
    usize xor_attr = attr_start(buf, pos, NFTA_BITWISE_XOR | NLA_F_NESTED);
    push_data_value(buf, pos, zero, sizeof(zero));
    attr_end(buf, pos, xor_attr);
    attr_end(buf, pos, data);
    attr_end(buf, pos, expr);
}

static void append_newrule_masq(unsigned char *buf, usize *pos, unsigned seq) {
    static const unsigned char src[4] = {172, 18, 0, 0};
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_NEWRULE),
                            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_APPEND, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_RULE_TABLE, "nat");
    push_attr_string(buf, pos, NFTA_RULE_CHAIN, "postrouting");
    usize exprs = attr_start(buf, pos, NFTA_RULE_EXPRESSIONS | NLA_F_NESTED);
    push_expr_meta(buf, pos, 1, NFT_REG32_00, NFT_META_OIFNAME);
    push_expr_cmp(buf, pos, 2, NFT_REG32_00, (const unsigned char *)"docker0", 8);
    push_expr_payload(buf, pos, 3, NFT_REG32_00 + 1, NFT_PAYLOAD_NETWORK_HEADER, 12, 4);
    push_expr_bitwise_mask(buf, pos, 4, NFT_REG32_00 + 1, NFT_REG32_00 + 2);
    push_expr_cmp(buf, pos, 5, NFT_REG32_00 + 2, src, sizeof(src));
    push_expr_empty(buf, pos, 6, "masq");
    attr_end(buf, pos, exprs);
    nlmsg_end(buf, pos, msg);
}

static void append_get(unsigned char *buf, usize *pos, unsigned op, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(op), NLM_F_REQUEST | 0x0300, seq);
    push_nfgenmsg(buf, pos);
    nlmsg_end(buf, pos, msg);
}

static void append_delrule(unsigned char *buf, usize *pos, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_DELRULE),
                            NLM_F_REQUEST | NLM_F_ACK, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_RULE_TABLE, "nat");
    push_attr_string(buf, pos, NFTA_RULE_CHAIN, "postrouting");
    push_attr_u64(buf, pos, NFTA_RULE_HANDLE, 1);
    nlmsg_end(buf, pos, msg);
}

static void append_delchain(unsigned char *buf, usize *pos, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_DELCHAIN),
                            NLM_F_REQUEST | NLM_F_ACK, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_CHAIN_TABLE, "nat");
    push_attr_string(buf, pos, NFTA_CHAIN_NAME, "postrouting");
    nlmsg_end(buf, pos, msg);
}

static void append_deltable(unsigned char *buf, usize *pos, unsigned seq) {
    usize msg = nlmsg_start(buf, pos, nft_msg(NFT_MSG_DELTABLE),
                            NLM_F_REQUEST | NLM_F_ACK, seq);
    push_nfgenmsg(buf, pos);
    push_attr_string(buf, pos, NFTA_TABLE_NAME, "nat");
    nlmsg_end(buf, pos, msg);
}

static int contains(const unsigned char *buf, usize len, const char *needle) {
    usize nlen = cstrlen(needle);
    if (nlen == 0 || len < nlen) {
        return 0;
    }
    for (usize i = 0; i + nlen <= len; i++) {
        usize j = 0;
        while (j < nlen && buf[i + j] == (unsigned char)needle[j]) {
            j++;
        }
        if (j == nlen) {
            return 1;
        }
    }
    return 0;
}

static void send_request(long fd, unsigned char *req, usize len) {
    unsigned char sockaddr_nl[12];
    for (usize i = 0; i < sizeof(sockaddr_nl); i++) {
        sockaddr_nl[i] = 0;
    }
    put_u16(sockaddr_nl, 0, AF_NETLINK);
    long sent = sys_sendto(fd, req, len, 0, sockaddr_nl, sizeof(sockaddr_nl));
    if (sent != (long)len) {
        fail_lit("nft-probe-send-fail\n");
    }
}

static int drain_responses(long fd, int want_masq) {
    unsigned char resp[1024];
    int saw_ack = 0;
    int saw_done = 0;
    int saw_masq = 0;
    for (;;) {
        long got = sys_recvfrom(fd, resp, sizeof(resp), MSG_DONTWAIT, 0, 0);
        if (got == -EAGAIN_VALUE) {
            break;
        }
        if (got < 20) {
            fail_lit("nft-probe-recv-fail\n");
        }
        int kind = get_u16(resp, 4);
        if (kind == NLMSG_ERROR) {
            int code = get_i32(resp, 16);
            if (code != 0) {
                fail_lit("nft-probe-nlmsg-error\n");
            }
            saw_ack = 1;
        }
        if (kind == NLMSG_DONE) {
            saw_done = 1;
        }
        if (contains(resp, (usize)got, "masquerade")) {
            saw_masq = 1;
        }
    }
    if (want_masq && !saw_masq) {
        fail_lit("nft-probe-missing-rule-dump\n");
    }
    return saw_ack || saw_done || saw_masq;
}

void start_c(void) {
    unsigned char req[2048];
    usize pos = 0;
    long fd = sys_socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, NETLINK_NETFILTER);
    if (fd < 0) {
        fail_lit("nft-probe-socket-fail\n");
    }

    puts_lit("nft-probe-start\n");

    append_batch_marker(req, &pos, NFNL_MSG_BATCH_BEGIN, 1);
    append_newtable(req, &pos, 2);
    append_newchain(req, &pos, 3);
    append_newrule_masq(req, &pos, 4);
    append_batch_marker(req, &pos, NFNL_MSG_BATCH_END, 5);
    send_request(fd, req, pos);
    if (!drain_responses(fd, 0)) {
        fail_lit("nft-probe-create-no-response\n");
    }
    puts_lit("nft-probe-create-ok\n");

    pos = 0;
    append_get(req, &pos, NFT_MSG_GETTABLE, 6);
    append_get(req, &pos, NFT_MSG_GETCHAIN, 7);
    append_get(req, &pos, NFT_MSG_GETRULE, 8);
    send_request(fd, req, pos);
    if (!drain_responses(fd, 1)) {
        fail_lit("nft-probe-dump-no-response\n");
    }
    puts_lit("nft-probe-dump-ok\n");

    pos = 0;
    append_batch_marker(req, &pos, NFNL_MSG_BATCH_BEGIN, 9);
    append_delrule(req, &pos, 10);
    append_delchain(req, &pos, 11);
    append_deltable(req, &pos, 12);
    append_batch_marker(req, &pos, NFNL_MSG_BATCH_END, 13);
    send_request(fd, req, pos);
    if (!drain_responses(fd, 0)) {
        fail_lit("nft-probe-delete-no-response\n");
    }
    puts_lit("nft-probe-delete-ok\n");

    puts_lit("nft-probe-success\n");
    sys_exit(0);
}

void _start(void) __attribute__((naked));
void _start(void) {
    asm volatile("tail start_c\n");
}
