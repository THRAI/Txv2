use super::*;

use alloc::vec::Vec;

const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_DUMP: u16 = 0x0300;
const NLMSG_DONE: u16 = 3;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_GETTABLE: u16 = 1;
const NFT_MSG_GETCHAIN: u16 = 4;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;

#[test]
fn nfnetlink_dumps_tables_chains_and_rules_from_staging_netfilter_model() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    reset_netfilter_for_test();

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

    let responses =
        crate::net::nfnetlink_handle_request(&nlmsg(nft_msg(NFT_MSG_NEWTABLE), 0x14, nfgenmsg()));

    assert_eq!(nlmsg_type(&responses[0]), 2);
    assert_eq!(
        i32::from_le_bytes(responses[0][16..20].try_into().unwrap()),
        -95
    );
}

fn nfgenmsg() -> Vec<u8> {
    let mut out = Vec::new();
    out.push(crate::net::NFPROTO_IPV4);
    out.push(0);
    out.extend_from_slice(&0u16.to_be_bytes());
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
