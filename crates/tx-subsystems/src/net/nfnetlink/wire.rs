use super::*;

pub(super) fn nfgenmsg(family: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(NFGENMSG_LEN);
    out.push(family);
    out.push(NFNETLINK_V0);
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

pub(super) fn nft_msg(op: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | op
}

pub(super) fn nfnl_subsys(kind: u16) -> u16 {
    (kind & 0xff00) >> 8
}

pub(super) fn nfnl_msg_type(kind: u16) -> u16 {
    kind & 0x00ff
}

pub(super) fn build_nlmsg(kind: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let msg_len = NLMSG_HDR_LEN + payload.len();
    let mut out = Vec::with_capacity(align4(msg_len));
    out.extend_from_slice(&(msg_len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(&mut out);
    out
}

pub(super) fn build_done_message(seq: u32, pid: u32) -> Vec<u8> {
    build_nlmsg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes())
}

pub(super) fn ack_or_error(header: NlMsgHeader, result: Result<(), Errno>) -> Vec<u8> {
    match result {
        Ok(()) => build_ack_response(header),
        Err(errno) => build_error_response(Some(header), errno),
    }
}

pub(super) fn build_ack_response(header: NlMsgHeader) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0i32.to_le_bytes());
    push_original_header(&mut payload, header);
    build_nlmsg(NLMSG_ERROR, 0, header.seq, header.pid, &payload)
}

pub(super) fn build_error_response(header: Option<NlMsgHeader>, errno: Errno) -> Vec<u8> {
    let mut payload = Vec::new();
    let error = -linux_errno_i32(errno);
    payload.extend_from_slice(&error.to_le_bytes());
    if let Some(header) = header {
        push_original_header(&mut payload, header);
        build_nlmsg(NLMSG_ERROR, 0, header.seq, header.pid, &payload)
    } else {
        build_nlmsg(NLMSG_ERROR, 0, 0, 0, &payload)
    }
}

pub(super) fn push_original_header(out: &mut Vec<u8>, header: NlMsgHeader) {
    out.extend_from_slice(&header.len.to_le_bytes());
    out.extend_from_slice(&header.kind.to_le_bytes());
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.pid.to_le_bytes());
}

pub(super) fn push_attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = NLA_HDR_LEN + payload.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(out);
}

pub(super) fn push_nested_attr(out: &mut Vec<u8>, kind: u16, build: impl FnOnce(&mut Vec<u8>)) {
    let mut nested = Vec::new();
    build(&mut nested);
    push_attr(out, kind | NLA_F_NESTED, &nested);
}

pub(super) fn push_attr_string(out: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = Vec::from(value.as_bytes());
    payload.push(0);
    push_attr(out, kind, &payload);
}

pub(super) fn push_attr_u32(out: &mut Vec<u8>, kind: u16, value: u32) {
    push_attr(out, kind, &value.to_be_bytes());
}

pub(super) fn push_attr_u64(out: &mut Vec<u8>, kind: u16, value: u64) {
    push_attr(out, kind, &value.to_be_bytes());
}

pub(super) fn parse_nfmsg_attrs(payload: &[u8]) -> Result<Vec<NlAttr<'_>>, Errno> {
    if payload.len() < NFGENMSG_LEN {
        return Err(Errno::EINVAL);
    }
    parse_attrs(&payload[NFGENMSG_LEN..])
}

pub(super) fn parse_attrs(bytes: &[u8]) -> Result<Vec<NlAttr<'_>>, Errno> {
    let mut attrs = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < NLA_HDR_LEN {
            return Err(Errno::EINVAL);
        }
        let len = read_u16(bytes, offset) as usize;
        if len < NLA_HDR_LEN || offset.saturating_add(len) > bytes.len() {
            return Err(Errno::EINVAL);
        }
        attrs.push(NlAttr {
            kind: read_u16(bytes, offset + 2) & NLA_TYPE_MASK,
            payload: &bytes[offset + NLA_HDR_LEN..offset + len],
        });
        offset += align4(len);
    }
    Ok(attrs)
}

pub(super) fn attr_payload<'a>(attrs: &'a [NlAttr<'a>], kind: u16) -> Option<&'a [u8]> {
    attrs
        .iter()
        .find(|attr| attr.kind == kind)
        .map(|attr| attr.payload)
}

pub(super) fn attr_string<'a>(attrs: &'a [NlAttr<'a>], kind: u16) -> Option<&'a str> {
    let bytes = attr_payload(attrs, kind)?;
    core::str::from_utf8(trim_nul(bytes)).ok()
}

pub(super) fn attr_u32(attrs: &[NlAttr<'_>], kind: u16) -> Option<u32> {
    let payload = attr_payload(attrs, kind)?;
    (payload.len() >= 4).then(|| decode_attr_u32(payload))
}

pub(super) fn attr_u64(attrs: &[NlAttr<'_>], kind: u16) -> Option<u64> {
    let payload = attr_payload(attrs, kind)?;
    (payload.len() >= 8).then(|| decode_attr_u64(payload))
}

pub(super) fn attr_nested_data_value<'a>(attrs: &'a [NlAttr<'a>], kind: u16) -> Option<&'a [u8]> {
    let bytes = attr_payload(attrs, kind)?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < NLA_HDR_LEN {
            return None;
        }
        let len = read_u16(bytes, offset) as usize;
        if len < NLA_HDR_LEN || offset.saturating_add(len) > bytes.len() {
            return None;
        }
        let nested_kind = read_u16(bytes, offset + 2) & NLA_TYPE_MASK;
        if nested_kind == NFTA_DATA_VALUE {
            return Some(&bytes[offset + NLA_HDR_LEN..offset + len]);
        }
        offset += align4(len);
    }
    None
}

pub(super) fn table_message_payload(family: u8, name: &str) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_TABLE_NAME, name);
    payload
}

pub(super) fn chain_message_payload(
    family: u8,
    table: &str,
    name: &str,
    hook: NetfilterHook,
) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_CHAIN_TABLE, table);
    push_attr_string(&mut payload, NFTA_CHAIN_NAME, name);
    push_attr_string(&mut payload, NFTA_CHAIN_TYPE, table_type_name(table));
    push_nested_attr(&mut payload, NFTA_CHAIN_HOOK, |nested| {
        push_attr_u32(nested, NFTA_HOOK_HOOKNUM, hooknum(hook));
        push_attr_u32(
            nested,
            NFTA_HOOK_PRIORITY,
            default_chain_priority(table, name) as u32,
        );
    });
    payload
}

pub(super) fn parse_chain_hook(payload: &[u8]) -> Result<(NetfilterHook, i32), Errno> {
    let attrs = parse_attrs(payload)?;
    let hooknum = attr_u32(&attrs, NFTA_HOOK_HOOKNUM).ok_or(Errno::EINVAL)?;
    let priority = attr_i32(&attrs, NFTA_HOOK_PRIORITY).unwrap_or(0);
    Ok((hook_from_hooknum(hooknum)?, priority))
}

pub(super) fn bind_register(regs: &mut Vec<RegisterBinding>, reg: u32, kind: RegisterBindingKind) {
    if let Some(existing) = regs.iter_mut().find(|binding| binding.reg == reg) {
        existing.kind = kind;
        return;
    }
    regs.push(RegisterBinding { reg, kind });
}

pub(super) fn register_binding(regs: &[RegisterBinding], reg: u32) -> Option<RegisterBindingKind> {
    regs.iter()
        .find(|binding| binding.reg == reg)
        .map(|binding| binding.kind.clone())
}

pub(super) fn register_value(regs: &[RegisterBinding], reg: u32) -> Option<&[u8]> {
    regs.iter()
        .find(|binding| binding.reg == reg)
        .and_then(|binding| match &binding.kind {
            RegisterBindingKind::Value(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
}

pub(super) fn protocol_from_data(value: &[u8]) -> Result<NetfilterConntrackProtocol, Errno> {
    match value.first().copied().ok_or(Errno::EINVAL)? {
        1 => Ok(NetfilterConntrackProtocol::Icmp),
        6 => Ok(NetfilterConntrackProtocol::Tcp),
        17 => Ok(NetfilterConntrackProtocol::Udp),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

pub(super) fn protocol_from_name(value: &str) -> Result<NetfilterConntrackProtocol, Errno> {
    match value {
        "icmp" | "ICMP" => Ok(NetfilterConntrackProtocol::Icmp),
        "tcp" | "TCP" => Ok(NetfilterConntrackProtocol::Tcp),
        "udp" | "UDP" => Ok(NetfilterConntrackProtocol::Udp),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn target_from_name(value: &str) -> Result<NetfilterTarget, Errno> {
    match value {
        "accept" | "ACCEPT" => Ok(NetfilterTarget::Accept),
        "drop" | "DROP" => Ok(NetfilterTarget::Drop),
        "masquerade" | "MASQUERADE" => Ok(NetfilterTarget::Masquerade),
        "dnat" | "DNAT" => Ok(NetfilterTarget::Dnat),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ipv4_from_data(value: &[u8]) -> Result<Ipv4Address, Errno> {
    if value.len() < 4 {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4Address::new([value[0], value[1], value[2], value[3]]))
}

pub(super) fn ipv4_cidr_from_payload_data(
    value: &[u8],
    len: u32,
    prefix_len: Option<u8>,
) -> Result<NetfilterIpv4Cidr, Errno> {
    if !(1..=4).contains(&len) || value.len() < len as usize {
        return Err(Errno::EINVAL);
    }
    let mut octets = [0u8; 4];
    octets[..len as usize].copy_from_slice(&value[..len as usize]);
    Ok(NetfilterIpv4Cidr {
        addr: Ipv4Address::new(octets),
        prefix_len: prefix_len.unwrap_or((len as u8) * 8),
    })
}

pub(super) fn u16_from_be_data(value: &[u8]) -> Result<u16, Errno> {
    if value.len() < 2 {
        return Err(Errno::EINVAL);
    }
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

pub(super) fn ipv4_mask_prefix_len(mask: &[u8]) -> Option<u8> {
    if mask.len() != 4 {
        return None;
    }
    let mut prefix = 0u8;
    let mut saw_zero = false;
    for byte in mask {
        for bit in (0..8).rev() {
            let set = (*byte & (1 << bit)) != 0;
            if set && saw_zero {
                return None;
            }
            if set {
                prefix += 1;
            } else {
                saw_zero = true;
            }
        }
    }
    Some(prefix)
}

pub(super) fn ipv4_prefix_mask(prefix_len: u8) -> [u8; 4] {
    let mut mask = [0u8; 4];
    for bit in 0..prefix_len.min(32) {
        mask[(bit / 8) as usize] |= 1 << (7 - (bit % 8));
    }
    mask
}

pub(super) fn leak_ascii_nul_string(value: &[u8]) -> Result<&'static str, Errno> {
    let text = core::str::from_utf8(trim_nul(value)).map_err(|_| Errno::EINVAL)?;
    Ok(leak_str(text))
}

pub(super) fn leak_str(value: &str) -> &'static str {
    Box::leak(value.to_string().into_boxed_str())
}

pub(super) fn trim_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte == 0) {
        Some(idx) => &bytes[..idx],
        None => bytes,
    }
}

pub(super) fn netfilter_table_from_name(name: &str) -> Result<NetfilterTable, Errno> {
    match name {
        "filter" | "FILTER" => Ok(NetfilterTable::Filter),
        "nat" | "NAT" => Ok(NetfilterTable::Nat),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

pub(super) fn hook_from_chain(
    netns: &NetNamespacePayload,
    table: &str,
    chain: &str,
) -> Result<NetfilterHook, Errno> {
    if let Some(stored) = nft_state_snapshot(netns)
        .1
        .iter()
        .find(|stored| stored.table == table && stored.name == chain)
        .map(|stored| stored.hook)
    {
        return Ok(stored);
    }
    hook_from_name(chain)
}

pub(super) fn hook_from_name(name: &str) -> Result<NetfilterHook, Errno> {
    match name {
        "PREROUTING" | "prerouting" => Ok(NetfilterHook::Prerouting),
        "INPUT" | "input" => Ok(NetfilterHook::Input),
        "FORWARD" | "forward" => Ok(NetfilterHook::Forward),
        "OUTPUT" | "output" => Ok(NetfilterHook::Output),
        "POSTROUTING" | "postrouting" => Ok(NetfilterHook::Postrouting),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn hook_from_hooknum(value: u32) -> Result<NetfilterHook, Errno> {
    match value {
        0 => Ok(NetfilterHook::Prerouting),
        1 => Ok(NetfilterHook::Input),
        2 => Ok(NetfilterHook::Forward),
        3 => Ok(NetfilterHook::Output),
        4 => Ok(NetfilterHook::Postrouting),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn default_chain_priority(table: &str, chain: &str) -> i32 {
    match (table, chain) {
        ("nat", "PREROUTING") | ("nat", "prerouting") => -100,
        ("nat", "INPUT") | ("nat", "input") => 100,
        ("nat", "OUTPUT") | ("nat", "output") => -100,
        ("nat", "POSTROUTING") | ("nat", "postrouting") => 100,
        _ => 0,
    }
}

pub(super) fn table_type_name(table: &str) -> &'static str {
    match table {
        "nat" | "NAT" => "nat",
        _ => "filter",
    }
}

pub(super) fn cidr_from_text(value: &str) -> Result<NetfilterIpv4Cidr, Errno> {
    let (addr, prefix) = value.split_once('/').unwrap_or((value, "32"));
    let prefix_len = parse_u8_text(prefix)?;
    if prefix_len > 32 {
        return Err(Errno::EINVAL);
    }
    Ok(NetfilterIpv4Cidr {
        addr: ipv4_from_text(addr)?,
        prefix_len,
    })
}

pub(super) fn ipv4_from_text(value: &str) -> Result<Ipv4Address, Errno> {
    let mut octets = [0u8; 4];
    let mut parts = value.split('.');
    for octet in &mut octets {
        *octet = parse_u8_text(parts.next().ok_or(Errno::EINVAL)?)?;
    }
    if parts.next().is_some() {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4Address::new(octets))
}

pub(super) fn parse_u8_text(value: &str) -> Result<u8, Errno> {
    let parsed = parse_usize_text(value)?;
    if parsed > u8::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(parsed as u8)
}

pub(super) fn parse_u16_text(value: &str) -> Result<u16, Errno> {
    let parsed = parse_usize_text(value)?;
    if parsed > u16::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(parsed as u16)
}

pub(super) fn parse_usize_text(value: &str) -> Result<usize, Errno> {
    let mut out = 0usize;
    if value.is_empty() {
        return Err(Errno::EINVAL);
    }
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(Errno::EINVAL);
        }
        out = out
            .checked_mul(10)
            .and_then(|cur| cur.checked_add((byte - b'0') as usize))
            .ok_or(Errno::EINVAL)?;
    }
    Ok(out)
}

pub(super) fn parse_nlmsg_header(bytes: &[u8]) -> Option<NlMsgHeader> {
    if bytes.len() < NLMSG_HDR_LEN {
        return None;
    }
    Some(NlMsgHeader {
        len: read_u32(bytes, 0),
        kind: read_u16(bytes, 4),
        flags: read_u16(bytes, 6),
        seq: read_u32(bytes, 8),
        pid: read_u32(bytes, 12),
    })
}

pub(super) fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

pub(super) fn attr_i32(attrs: &[NlAttr<'_>], kind: u16) -> Option<i32> {
    let payload = attr_payload(attrs, kind)?;
    (payload.len() >= 4).then(|| decode_attr_i32(payload))
}

pub(super) fn decode_attr_u32(payload: &[u8]) -> u32 {
    let le = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let be = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    prefer_small_u32(le, be)
}

pub(super) fn decode_attr_i32(payload: &[u8]) -> i32 {
    let le = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let be = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    prefer_small_i32(le, be)
}

pub(super) fn decode_attr_u64(payload: &[u8]) -> u64 {
    let le = u64::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
        payload[7],
    ]);
    let be = u64::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
        payload[7],
    ]);
    prefer_small_u64(le, be)
}

pub(super) fn prefer_small_u32(le: u32, be: u32) -> u32 {
    match (le <= 0xffff, be <= 0xffff) {
        (true, false) => le,
        (false, true) => be,
        _ => le,
    }
}

pub(super) fn prefer_small_u64(le: u64, be: u64) -> u64 {
    match (le <= 0xffff_ffff, be <= 0xffff_ffff) {
        (true, false) => le,
        (false, true) => be,
        _ => le,
    }
}

pub(super) fn prefer_small_i32(le: i32, be: i32) -> i32 {
    let le_small = le.abs() <= 1_000_000;
    let be_small = be.abs() <= 1_000_000;
    match (le_small, be_small) {
        (true, false) => le,
        (false, true) => be,
        _ => le,
    }
}

pub(super) fn align4(len: usize) -> usize {
    (len + 3) & !3
}

pub(super) fn pad_to_align4(out: &mut Vec<u8>) {
    while out.len() != align4(out.len()) {
        out.push(0);
    }
}

pub(super) fn linux_errno_i32(errno: Errno) -> i32 {
    match errno {
        Errno::EINVAL => 22,
        Errno::ENOENT => 2,
        Errno::ENOTCONN => 107,
        Errno::EOPNOTSUPP => 95,
        _ => 5,
    }
}
