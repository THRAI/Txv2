use super::*;

impl EtherIface {
    pub(super) fn prepare_ipv4_ingress<'a>(&self, packet: &'a [u8]) -> Ipv4IngressOutcome<'a> {
        let Some(meta) = parse_ipv4_meta(packet) else {
            return Ipv4IngressOutcome::Malformed;
        };
        if !meta.is_fragmented() {
            return Ipv4IngressOutcome::Complete(Ipv4IngressPacket::Borrowed(
                &packet[..meta.total_len],
            ));
        }
        if meta.more_fragments && meta.payload_len() % 8 != 0 {
            return Ipv4IngressOutcome::Malformed;
        }
        if meta.fragment_end() > IPV4_MAX_PACKET_LEN {
            return Ipv4IngressOutcome::Malformed;
        }

        match self.ingest_ipv4_fragment(packet, meta) {
            Some(packet) => Ipv4IngressOutcome::Complete(Ipv4IngressPacket::Owned(packet)),
            None => Ipv4IngressOutcome::Pending,
        }
    }

    pub(super) fn ingest_ipv4_fragment(
        &self,
        packet: &[u8],
        meta: Ipv4PacketMeta,
    ) -> Option<Vec<u8>> {
        let key = Ipv4FragmentKey {
            src: meta.src,
            dst: meta.dst,
            ident: meta.ident,
            protocol: meta.protocol,
        };

        let now = crate::net::clock::net_now_instant();
        let mut fragments = self.ipv4_fragments.lock();
        expire_and_cap_ipv4_fragments(&mut fragments, &key, now);

        let entry = fragments.entry(key).or_default();
        entry.last_seen = now;
        if meta.fragment_offset == 0 {
            entry.header = Some(packet[..meta.header_len].to_vec());
        }

        let payload = &packet[meta.header_len..meta.total_len];
        let end = meta.fragment_offset + payload.len();
        if entry.payload.len() < end {
            entry.payload.resize(end, 0);
        }
        entry.payload[meta.fragment_offset..end].copy_from_slice(payload);
        entry.record_range(meta.fragment_offset, end);
        if !meta.more_fragments {
            entry.total_payload_len = Some(end);
        }

        if !entry.is_complete() {
            return None;
        }

        let entry = fragments.remove(&key)?;
        assemble_ipv4_packet(entry)
    }

    pub(super) fn transmit_ipv4_fragments(
        &self,
        dst_mac: EthernetAddress,
        packet: &[u8],
        guard: &Guard<'_>,
    ) -> PacketTxResult {
        let Some(meta) = parse_ipv4_meta(packet) else {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return PacketTxResult::Failed {
                errno: Errno::EINVAL,
            };
        };
        // smoltcp-emitted local packets carry DF with identification zero because
        // smoltcp does not fragment; once txKernel fragments here, that internal
        // default must not make ordinary large ping payloads fail.
        if meta.flags_fragment & IPV4_FLAG_DONT_FRAGMENT != 0 && meta.ident != 0 {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return PacketTxResult::Failed {
                errno: Errno::EMSGSIZE,
            };
        }

        let mtu = usize::from(self.common.mtu());
        if mtu <= meta.header_len + 8 {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return PacketTxResult::Failed {
                errno: Errno::EMSGSIZE,
            };
        }
        let fragment_payload_limit = ((mtu - meta.header_len) / 8) * 8;
        if fragment_payload_limit == 0 {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return PacketTxResult::Failed {
                errno: Errno::EMSGSIZE,
            };
        }

        let payload = &packet[meta.header_len..meta.total_len];
        let ident = if meta.ident == 0 {
            self.allocate_ipv4_ident()
        } else {
            meta.ident
        };
        let base_fragment_offset = meta.fragment_offset;
        let mut offset = 0usize;
        let mut tx_bytes = 0usize;

        while offset < payload.len() {
            let remaining = payload.len() - offset;
            let chunk_len = remaining.min(fragment_payload_limit);
            let more_fragments = offset + chunk_len < payload.len() || meta.more_fragments;
            let mut fragment = packet[..meta.header_len + chunk_len.min(remaining)].to_vec();
            fragment[..meta.header_len].copy_from_slice(&packet[..meta.header_len]);
            fragment[meta.header_len..].copy_from_slice(&payload[offset..offset + chunk_len]);

            let total_len = meta.header_len + chunk_len;
            write_u16(&mut fragment, 2, total_len as u16);
            write_u16(&mut fragment, 4, ident);
            let offset_units = ((base_fragment_offset + offset) / 8) as u16;
            let mut flags_fragment = meta.flags_fragment & IPV4_FLAG_RESERVED;
            if more_fragments {
                flags_fragment |= IPV4_FLAG_MORE_FRAGMENTS;
            }
            flags_fragment |= offset_units & IPV4_FRAGMENT_OFFSET_MASK;
            write_u16(&mut fragment, 6, flags_fragment);
            fill_ipv4_header_checksum(&mut fragment);

            let frame = build_ipv4_ethernet_frame(self.ether_addr, dst_mac, &fragment);
            match self.transmit_frame(&frame, guard) {
                PacketTxResult::Accepted { frame_len } => tx_bytes += frame_len,
                other => return other,
            }
            offset += chunk_len;
        }

        PacketTxResult::Accepted {
            frame_len: tx_bytes,
        }
    }

    pub(super) fn allocate_ipv4_ident(&self) -> u16 {
        let next = self.next_ipv4_ident.fetch_add(1, Ordering::Relaxed);
        (next as u16).max(1)
    }
}
