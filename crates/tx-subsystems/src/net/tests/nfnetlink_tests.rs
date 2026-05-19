use super::*;

use alloc::vec;
use alloc::vec::Vec;

use crate::net::{NetfilterRule, NetfilterTable, NetfilterTarget, NetfilterVerdict};

const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_DUMP: u16 = 0x0300;
const NLMSG_DONE: u16 = 3;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_GETTABLE: u16 = 1;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_GETCHAIN: u16 = 4;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_DELRULE: u16 = 8;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFTA_TABLE_NAME: u16 = 1;
const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_CHAIN_TYPE: u16 = 7;
const NFTA_HOOK_HOOKNUM: u16 = 1;
const NFTA_HOOK_PRIORITY: u16 = 2;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_META_DREG: u16 = 1;
const NFTA_META_KEY: u16 = 2;
const NFTA_PAYLOAD_DREG: u16 = 1;
const NFTA_PAYLOAD_BASE: u16 = 2;
const NFTA_PAYLOAD_OFFSET: u16 = 3;
const NFTA_PAYLOAD_LEN: u16 = 4;
const NFTA_BITWISE_SREG: u16 = 1;
const NFTA_BITWISE_DREG: u16 = 2;
const NFTA_BITWISE_LEN: u16 = 3;
const NFTA_BITWISE_MASK: u16 = 4;
const NFTA_BITWISE_XOR: u16 = 5;
const NFTA_CMP_SREG: u16 = 1;
const NFTA_CMP_OP: u16 = 2;
const NFTA_CMP_DATA: u16 = 3;
const NFTA_IMMEDIATE_DREG: u16 = 1;
const NFTA_IMMEDIATE_DATA: u16 = 2;
const NFTA_DATA_VALUE: u16 = 1;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_NAT_TYPE: u16 = 1;
const NFTA_NAT_FAMILY: u16 = 2;
const NFTA_NAT_REG_ADDR_MIN: u16 = 3;
const NFTA_NAT_REG_PROTO_MIN: u16 = 5;
const NFT_REG_VERDICT: u32 = 0;
const NFT_REG32_00: u32 = 8;
const NFT_CMP_EQ: u32 = 0;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
const NFT_META_OIFNAME: u32 = 7;
const NFT_NAT_DNAT: u32 = 1;
const NF_ACCEPT: u32 = 1;
const NFPROTO_IPV4_U32: u32 = 2;

#[test]
fn nfnetlink_dumps_tables_chains_and_rules_from_staging_netfilter_model() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    add_masquerade_rule_for_test_or_bootstrap(
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "uplink-nft0",
    )
    .expect("masquerade rule");
    add_dnat_rule_for_test_or_bootstrap(
        NetfilterConntrackProtocol::Tcp,
        Ipv4Address::new([10, 0, 2, 15]),
        8080,
        Ipv4Address::new([172, 17, 0, 2]),
        80,
    )
    .expect("dnat rule");

    let tables =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_GETTABLE), 0x11, nfgenmsg()));
    assert!(tables
        .iter()
        .any(|msg| nlmsg_type(msg) == nft_msg(NFT_MSG_NEWTABLE) && contains_bytes(msg, b"nat\0")));
    assert!(tables.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));

    let chains =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_GETCHAIN), 0x12, nfgenmsg()));
    assert!(chains.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWCHAIN) && contains_bytes(msg, b"postrouting\0")
    }));
    assert!(chains.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWCHAIN) && contains_bytes(msg, b"prerouting\0")
    }));

    let rules =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_GETRULE), 0x13, nfgenmsg()));
    assert!(rules.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"masquerade")
    }));
    assert!(rules
        .iter()
        .any(|msg| nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"dnat")));
}

#[test]
fn nfnetlink_rejects_mutation_messages_with_explicit_error() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    let responses =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_NEWTABLE), 0x14, nfgenmsg()));

    assert_eq!(nlmsg_type(&responses[0]), 2);
    assert_eq!(
        i32::from_le_bytes(responses[0][16..20].try_into().unwrap()),
        -22
    );
}

#[test]
fn nfnetlink_newtable_newchain_and_newrule_add_masquerade_rule() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    let table = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_NEWTABLE),
        0x21,
        table_payload("nat"),
    ));
    assert_ack_ok(&table[0]);
    let chain = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_NEWCHAIN),
        0x22,
        chain_payload("nat", "postrouting", 4, 100),
    ));
    assert_ack_ok(&chain[0]);

    let rule = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_NEWRULE),
        0x23,
        rule_payload(
            "nat",
            "postrouting",
            vec![
                expr_meta(NFT_REG32_00, NFT_META_OIFNAME),
                expr_cmp(NFT_REG32_00, b"docker0\0"),
                expr_payload(NFT_REG32_00 + 1, NFT_PAYLOAD_NETWORK_HEADER, 12, 4),
                expr_bitwise_ipv4_mask(NFT_REG32_00 + 1, NFT_REG32_00 + 2, [255, 255, 0, 0]),
                expr_cmp(NFT_REG32_00 + 2, &[172, 17, 0, 0]),
                expr_empty("masq"),
            ],
        ),
    ));
    assert_ack_ok(&rule[0]);

    let rules = netfilter_rules_snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].table, NetfilterTable::Nat);
    assert_eq!(rules[0].hook, NetfilterHook::Postrouting);
    assert_eq!(rules[0].target, NetfilterTarget::Masquerade);
    assert_eq!(rules[0].out_iface, Some("docker0"));
    assert_eq!(
        rules[0].src,
        Some(NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        })
    );

    let dump =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_GETRULE), 0x24, nfgenmsg()));
    assert!(dump.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"masquerade")
    }));
}

#[test]
fn nfnetlink_newrule_adds_dnat_and_delrule_removes_by_handle() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    assert_ack_ok(
        &crate::net::nfnetlink_handle_request(&nlmsg(
            nft_msg(NFT_MSG_NEWCHAIN),
            0x31,
            chain_payload("nat", "prerouting", 0, -100),
        ))[0],
    );

    let rule = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_NEWRULE),
        0x32,
        rule_payload(
            "nat",
            "prerouting",
            vec![
                expr_payload(NFT_REG32_00, NFT_PAYLOAD_NETWORK_HEADER, 9, 1),
                expr_cmp(NFT_REG32_00, &[6]),
                expr_payload(NFT_REG32_00 + 1, NFT_PAYLOAD_NETWORK_HEADER, 16, 4),
                expr_cmp(NFT_REG32_00 + 1, &[10, 0, 2, 15]),
                expr_payload(NFT_REG32_00 + 2, NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2),
                expr_cmp(NFT_REG32_00 + 2, &8080u16.to_be_bytes()),
                expr_immediate_value(NFT_REG32_00 + 3, &[172, 17, 0, 2]),
                expr_immediate_value(NFT_REG32_00 + 4, &80u16.to_be_bytes()),
                expr_nat_dnat(NFT_REG32_00 + 3, NFT_REG32_00 + 4),
            ],
        ),
    ));
    assert_ack_ok(&rule[0]);

    let rules = netfilter_rules_snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].target, NetfilterTarget::Dnat);
    assert_eq!(rules[0].protocol, Some(NetfilterConntrackProtocol::Tcp));
    assert_eq!(
        rules[0].dst,
        Some(NetfilterIpv4Cidr {
            addr: Ipv4Address::new([10, 0, 2, 15]),
            prefix_len: 32,
        })
    );
    assert_eq!(rules[0].dst_port, Some(8080));
    assert_eq!(rules[0].to_addr, Some(Ipv4Address::new([172, 17, 0, 2])));
    assert_eq!(rules[0].to_port, Some(80));

    let deleted = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_DELRULE),
        0x33,
        delrule_payload("nat", "prerouting", 1),
    ));
    assert_ack_ok(&deleted[0]);
    assert!(netfilter_rules_snapshot().is_empty());
}

#[test]
fn nfnetlink_newrule_adds_forward_accept_filter_rule() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    assert_ack_ok(
        &crate::net::nfnetlink_handle_request(&nlmsg(
            nft_msg(NFT_MSG_NEWCHAIN),
            0x41,
            chain_payload("filter", "forward", 2, 0),
        ))[0],
    );
    let rule = crate::net::nfnetlink_handle_request(&nlmsg(
        nft_msg(NFT_MSG_NEWRULE),
        0x42,
        rule_payload("filter", "forward", vec![expr_immediate_accept()]),
    ));
    assert_ack_ok(&rule[0]);

    let rules = netfilter_rules_snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].table, NetfilterTable::Filter);
    assert_eq!(rules[0].hook, NetfilterHook::Forward);
    assert_eq!(rules[0].target, NetfilterTarget::Accept);
}

#[test]
fn nfnetlink_batch_create_dump_and_delete_masquerade_rule() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    let mut create = Vec::new();
    create.extend_from_slice(&batch_marker(NFNL_MSG_BATCH_BEGIN, 0x61));
    create.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_NEWTABLE),
        0x62,
        table_payload("nat"),
    ));
    create.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_NEWCHAIN),
        0x63,
        chain_payload("nat", "postrouting", 4, 100),
    ));
    create.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_NEWRULE),
        0x64,
        rule_payload(
            "nat",
            "postrouting",
            vec![
                expr_meta(NFT_REG32_00, NFT_META_OIFNAME),
                expr_cmp(NFT_REG32_00, b"docker0\0"),
                expr_payload(NFT_REG32_00 + 1, NFT_PAYLOAD_NETWORK_HEADER, 12, 4),
                expr_bitwise_ipv4_mask(NFT_REG32_00 + 1, NFT_REG32_00 + 2, [255, 255, 0, 0]),
                expr_cmp(NFT_REG32_00 + 2, &[172, 18, 0, 0]),
                expr_empty("masq"),
            ],
        ),
    ));
    create.extend_from_slice(&batch_marker(NFNL_MSG_BATCH_END, 0x65));

    let create_responses = crate::net::nfnetlink_handle_request(&create);
    assert!(create_responses.iter().all(|msg| {
        nlmsg_type(msg) == 2 && i32::from_le_bytes(msg[16..20].try_into().unwrap()) == 0
    }));

    let rules = netfilter_rules_snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].target, NetfilterTarget::Masquerade);
    assert_eq!(rules[0].out_iface, Some("docker0"));
    assert_eq!(
        rules[0].src,
        Some(NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 18, 0, 0]),
            prefix_len: 16,
        })
    );

    let mut dump = Vec::new();
    dump.extend_from_slice(&nlmsg(nft_msg(NFT_MSG_GETTABLE), 0x66, nfgenmsg()));
    dump.extend_from_slice(&nlmsg(nft_msg(NFT_MSG_GETCHAIN), 0x67, nfgenmsg()));
    dump.extend_from_slice(&nlmsg(nft_msg(NFT_MSG_GETRULE), 0x68, nfgenmsg()));
    let dump_responses = crate::net::nfnetlink_handle_request(&dump);
    assert!(dump_responses
        .iter()
        .any(|msg| nlmsg_type(msg) == nft_msg(NFT_MSG_NEWTABLE) && contains_bytes(msg, b"nat\0")));
    assert!(dump_responses.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWCHAIN) && contains_bytes(msg, b"postrouting\0")
    }));
    assert!(dump_responses.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"masquerade")
    }));

    let mut delete = Vec::new();
    delete.extend_from_slice(&batch_marker(NFNL_MSG_BATCH_BEGIN, 0x69));
    delete.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_DELRULE),
        0x6a,
        delrule_payload("nat", "postrouting", 1),
    ));
    delete.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_DELCHAIN),
        0x6b,
        delchain_payload("nat", "postrouting"),
    ));
    delete.extend_from_slice(&nlmsg(
        nft_msg(NFT_MSG_DELTABLE),
        0x6c,
        deltable_payload("nat"),
    ));
    delete.extend_from_slice(&batch_marker(NFNL_MSG_BATCH_END, 0x6d));
    let delete_responses = crate::net::nfnetlink_handle_request(&delete);
    assert!(delete_responses.iter().all(|msg| {
        nlmsg_type(msg) == 2 && i32::from_le_bytes(msg[16..20].try_into().unwrap()) == 0
    }));
    assert!(netfilter_rules_snapshot().is_empty());
}

#[test]
fn netfilter_rule_counters_increment_on_filter_match() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();

    crate::net::add_netfilter_rule_for_test_or_bootstrap(NetfilterRule {
        table: NetfilterTable::Filter,
        hook: NetfilterHook::Forward,
        protocol: None,
        src: None,
        dst: None,
        dst_port: None,
        in_iface: Some("docker0"),
        out_iface: Some("uplink0"),
        target: NetfilterTarget::Drop,
        to_addr: None,
        to_port: None,
    })
    .expect("filter rule");

    let verdict = crate::net::run_frame_hook(
        NetfilterFrameContext {
            hook: NetfilterHook::Forward,
            bridge: None,
            ingress: Some("docker0"),
            egress: Some("uplink0"),
        },
        b"payload",
    );
    assert_eq!(verdict, NetfilterVerdict::Drop);

    let snapshots = crate::net::netfilter_rule_snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].counters.packets, 1);
    assert_eq!(snapshots[0].counters.bytes, 7);
    assert!(crate::net::proc_net_netfilter_rules_text().contains("0\t1\t7\tfilter"));
}

#[test]
fn nfnetlink_rules_are_scoped_to_target_network_namespace() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    reset_netfilter_for_test();
    crate::net::nfnetlink::reset_nfnetlink_for_test();

    let isolated =
        crate::net::create_isolated_net_namespace_for_test("nft-ns").expect("isolated namespace");
    let isolated = isolated.payload_cap().expect("isolated payload");

    assert_ack_ok(
        &crate::net::nfnetlink_handle_request_in_namespace(
            &isolated,
            &nlmsg(
                nft_msg(NFT_MSG_NEWCHAIN),
                0x51,
                chain_payload("filter", "forward", 2, 0),
            ),
        )[0],
    );
    assert_ack_ok(
        &crate::net::nfnetlink_handle_request_in_namespace(
            &isolated,
            &nlmsg(
                nft_msg(NFT_MSG_NEWRULE),
                0x52,
                rule_payload("filter", "forward", vec![expr_immediate_accept()]),
            ),
        )[0],
    );

    assert!(crate::net::netfilter_rules_snapshot().is_empty());
    let isolated_rules = crate::net::netfilter_rules_snapshot_for_namespace(&isolated);
    assert_eq!(isolated_rules.len(), 1);
    assert_eq!(isolated_rules[0].target, NetfilterTarget::Accept);

    let initial_dump =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_GETRULE), 0x53, nfgenmsg()));
    assert!(!initial_dump.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"accept")
    }));

    let isolated_dump = crate::net::nfnetlink_handle_request_in_namespace(
        &isolated,
        &nlmsg(nft_msg(NFT_MSG_GETRULE), 0x54, nfgenmsg()),
    );
    assert!(isolated_dump.iter().any(|msg| {
        nlmsg_type(msg) == nft_msg(NFT_MSG_NEWRULE) && contains_bytes(msg, b"accept")
    }));
}

fn nfgenmsg() -> Vec<u8> {
    let mut out = Vec::new();
    out.push(crate::net::NFPROTO_IPV4);
    out.push(0);
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn table_payload(name: &str) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_TABLE_NAME, name);
    out
}

fn chain_payload(table: &str, name: &str, hook: u32, priority: i32) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_CHAIN_TABLE, table);
    push_attr_string(&mut out, NFTA_CHAIN_NAME, name);
    push_attr_string(&mut out, NFTA_CHAIN_TYPE, table);
    push_nested_attr(&mut out, NFTA_CHAIN_HOOK, |nested| {
        push_attr_u32(nested, NFTA_HOOK_HOOKNUM, hook);
        push_attr_u32(nested, NFTA_HOOK_PRIORITY, priority as u32);
    });
    out
}

fn rule_payload(table: &str, chain: &str, expressions: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_RULE_TABLE, table);
    push_attr_string(&mut out, NFTA_RULE_CHAIN, chain);
    push_nested_attr(&mut out, NFTA_RULE_EXPRESSIONS, |nested| {
        for (idx, expr) in expressions.iter().enumerate() {
            push_attr(nested, idx as u16 + 1, expr);
        }
    });
    out
}

fn delrule_payload(table: &str, chain: &str, handle: u64) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_RULE_TABLE, table);
    push_attr_string(&mut out, NFTA_RULE_CHAIN, chain);
    push_attr_u64(&mut out, NFTA_RULE_HANDLE, handle);
    out
}

fn delchain_payload(table: &str, chain: &str) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_CHAIN_TABLE, table);
    push_attr_string(&mut out, NFTA_CHAIN_NAME, chain);
    out
}

fn deltable_payload(table: &str) -> Vec<u8> {
    let mut out = nfgenmsg();
    push_attr_string(&mut out, NFTA_TABLE_NAME, table);
    out
}

fn expr_empty(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, NFTA_EXPR_NAME, name);
    out
}

fn expr_meta(dreg: u32, key: u32) -> Vec<u8> {
    expr_with_data("meta", |data| {
        push_attr_u32(data, NFTA_META_DREG, dreg);
        push_attr_u32(data, NFTA_META_KEY, key);
    })
}

fn expr_payload(dreg: u32, base: u32, offset: u32, len: u32) -> Vec<u8> {
    expr_with_data("payload", |data| {
        push_attr_u32(data, NFTA_PAYLOAD_DREG, dreg);
        push_attr_u32(data, NFTA_PAYLOAD_BASE, base);
        push_attr_u32(data, NFTA_PAYLOAD_OFFSET, offset);
        push_attr_u32(data, NFTA_PAYLOAD_LEN, len);
    })
}

fn expr_bitwise_ipv4_mask(sreg: u32, dreg: u32, mask: [u8; 4]) -> Vec<u8> {
    expr_with_data("bitwise", |data| {
        push_attr_u32(data, NFTA_BITWISE_SREG, sreg);
        push_attr_u32(data, NFTA_BITWISE_DREG, dreg);
        push_attr_u32(data, NFTA_BITWISE_LEN, 4);
        push_nested_attr(data, NFTA_BITWISE_MASK, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, &mask);
        });
        push_nested_attr(data, NFTA_BITWISE_XOR, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, &[0, 0, 0, 0]);
        });
    })
}

fn expr_cmp(sreg: u32, value: &[u8]) -> Vec<u8> {
    expr_with_data("cmp", |data| {
        push_attr_u32(data, NFTA_CMP_SREG, sreg);
        push_attr_u32(data, NFTA_CMP_OP, NFT_CMP_EQ);
        push_nested_attr(data, NFTA_CMP_DATA, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, value);
        });
    })
}

fn expr_immediate_value(dreg: u32, value: &[u8]) -> Vec<u8> {
    expr_with_data("immediate", |data| {
        push_attr_u32(data, NFTA_IMMEDIATE_DREG, dreg);
        push_nested_attr(data, NFTA_IMMEDIATE_DATA, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, value);
        });
    })
}

fn expr_immediate_accept() -> Vec<u8> {
    expr_with_data("immediate", |data| {
        push_attr_u32(data, NFTA_IMMEDIATE_DREG, NFT_REG_VERDICT);
        push_nested_attr(data, NFTA_IMMEDIATE_DATA, |nested| {
            push_nested_attr(nested, NFTA_DATA_VERDICT, |verdict| {
                push_attr_u32(verdict, NFTA_VERDICT_CODE, NF_ACCEPT);
            });
        });
    })
}

fn expr_nat_dnat(addr_reg: u32, port_reg: u32) -> Vec<u8> {
    expr_with_data("nat", |data| {
        push_attr_u32(data, NFTA_NAT_TYPE, NFT_NAT_DNAT);
        push_attr_u32(data, NFTA_NAT_FAMILY, NFPROTO_IPV4_U32);
        push_attr_u32(data, NFTA_NAT_REG_ADDR_MIN, addr_reg);
        push_attr_u32(data, NFTA_NAT_REG_PROTO_MIN, port_reg);
    })
}

fn expr_with_data(name: &str, build: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, NFTA_EXPR_NAME, name);
    push_nested_attr(&mut out, NFTA_EXPR_DATA, build);
    out
}

fn nlmsg(kind: u16, seq: u32, payload: Vec<u8>) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn batch_marker(kind: u16, seq: u32) -> Vec<u8> {
    nlmsg(kind, seq, nfgenmsg())
}

fn nft_msg(op: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | op
}

fn nlmsg_type(msg: &[u8]) -> u16 {
    u16::from_le_bytes([msg[4], msg[5]])
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn assert_ack_ok(msg: &[u8]) {
    assert_eq!(nlmsg_type(msg), 2);
    assert_eq!(i32::from_le_bytes(msg[16..20].try_into().unwrap()), 0);
}

fn push_nested_attr(out: &mut Vec<u8>, kind: u16, build: impl FnOnce(&mut Vec<u8>)) {
    let mut nested = Vec::new();
    build(&mut nested);
    push_attr(out, kind | 0x8000, &nested);
}

fn push_attr_string(out: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = Vec::from(value.as_bytes());
    payload.push(0);
    push_attr(out, kind, &payload);
}

fn push_attr_u32(out: &mut Vec<u8>, kind: u16, value: u32) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn push_attr_u64(out: &mut Vec<u8>, kind: u16, value: u64) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn push_attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}
