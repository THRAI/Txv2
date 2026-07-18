use super::*;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv6Packet, Icmpv6Repr, IpProtocol, IpRepr, Ipv6Repr, NdiscNeighborFlags, NdiscRepr,
};

impl EtherIface {
    pub(super) fn process_arp(
        &self,
        payload: &[u8],
        now: Instant,
        guard: Option<&Guard<'_>>,
    ) -> PacketDispatch {
        let packet = match ArpPacket::new_checked(payload) {
            Ok(packet) => packet,
            Err(_) => {
                self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketDispatch::Malformed;
            }
        };
        let repr = match ArpRepr::parse(&packet) {
            Ok(repr) => repr,
            Err(_) => {
                self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketDispatch::Malformed;
            }
        };

        match repr {
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr,
                source_protocol_addr,
                ..
            } => {
                if source_hardware_addr.is_unicast() {
                    self.learn_arp(
                        from_smoltcp_ipv4(source_protocol_addr),
                        from_smoltcp_ether(source_hardware_addr),
                        now,
                    );
                }
                PacketDispatch::Unsupported
            }
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr,
                source_protocol_addr,
                target_protocol_addr,
                ..
            } => {
                if source_hardware_addr.is_unicast() {
                    self.learn_arp(
                        from_smoltcp_ipv4(source_protocol_addr),
                        from_smoltcp_ether(source_hardware_addr),
                        now,
                    );
                }
                if from_smoltcp_ipv4(target_protocol_addr) == self.common.ipv4_addr() {
                    if let Some(guard) = guard {
                        let reply = self.build_arp_reply(
                            from_smoltcp_ipv4(source_protocol_addr),
                            from_smoltcp_ether(source_hardware_addr),
                        );
                        if matches!(
                            self.transmit_frame(&reply, guard),
                            PacketTxResult::Accepted { .. }
                        ) {
                            self.arp_stats.replies_tx.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                PacketDispatch::Unsupported
            }
            _ => PacketDispatch::Unsupported,
        }
    }

    pub(super) fn learn_arp(&self, ip: Ipv4Address, mac: EthernetAddress, now: Instant) {
        self.arp_table.lock().insert(
            ip,
            ArpEntry {
                mac,
                expires_at: now + ARP_CACHE_TTL,
            },
        );
        self.pending_arp.lock().remove(&ip);
        self.arp_stats.resolved.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn lookup_arp_entry(&self, ip: Ipv4Address, now: Instant) -> Option<ArpEntry> {
        let mut table = self.arp_table.lock();
        match table.get(&ip).copied() {
            Some(entry) if entry.expires_at > now => Some(entry),
            Some(_) => {
                table.remove(&ip);
                None
            }
            None => None,
        }
    }

    pub(super) fn resolve_or_request(&self, next_hop: Ipv4Address, now: Instant) -> ArpResolution {
        if next_hop == Ipv4Address::BROADCAST {
            return ArpResolution::Resolved {
                mac: EthernetAddress::BROADCAST,
            };
        }

        if let Some(entry) = self.lookup_arp_entry(next_hop, now) {
            self.arp_stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return ArpResolution::Resolved { mac: entry.mac };
        }

        self.arp_stats.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.queue_pending_arp(next_hop, now)
    }

    pub(super) fn queue_pending_arp(&self, ip: Ipv4Address, now: Instant) -> ArpResolution {
        let mut pending = self.pending_arp.lock();
        match pending.get(&ip).copied() {
            Some(ArpPendingEntry {
                last_error: Some(errno),
                ..
            }) => ArpResolution::Failed {
                next_hop: ip,
                errno,
            },
            Some(_) => ArpResolution::Pending { next_hop: ip },
            None => {
                pending.insert(
                    ip,
                    ArpPendingEntry {
                        ip,
                        attempts: 0,
                        next_probe_at: now,
                        last_error: None,
                    },
                );
                ArpResolution::Pending { next_hop: ip }
            }
        }
    }

    pub(super) fn ready_pending_arp(&self, now: Instant, budget: usize) -> Vec<Ipv4Address> {
        self.pending_arp
            .lock()
            .iter()
            .filter_map(|(ip, entry)| {
                (entry.last_error.is_none() && entry.next_probe_at <= now).then_some(*ip)
            })
            .take(budget)
            .collect()
    }

    pub(super) fn pending_entry_for_probe(
        &self,
        ip: Ipv4Address,
        now: Instant,
    ) -> Option<ArpPendingEntry> {
        let mut pending = self.pending_arp.lock();
        let entry = pending.get_mut(&ip)?;
        if entry.last_error.is_some() || entry.next_probe_at > now {
            return None;
        }
        if entry.attempts >= ARP_REQUEST_RETRY_LIMIT {
            entry.last_error = Some(Errno::EADDRNOTAVAIL);
            self.arp_stats
                .retry_limit_exceeded
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(*entry)
    }

    pub(super) fn mark_arp_probe_sent(&self, ip: Ipv4Address, now: Instant) {
        if let Some(entry) = self.pending_arp.lock().get_mut(&ip) {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.next_probe_at = now + ARP_REQUEST_RETRY_DELAY;
        }
    }

    pub(super) fn accepts_ethernet_destination(&self, dst: SmoltcpEthernetAddress) -> bool {
        if dst.is_broadcast() || dst == to_smoltcp_ether(self.ether_addr) {
            return true;
        }
        // IPv6 V2: also accept our solicited-node multicast MAC so external
        // neighbours can solicit us (inbound NS → we answer NA). Without this,
        // the multicast-addressed NS would be dropped and D3 could never fire.
        if let Some(local6) = self.common.ipv6_addr() {
            if dst == to_smoltcp_ether(multicast_mac_for(solicited_node_multicast(local6))) {
                return true;
            }
        }
        false
    }

    pub(super) fn accepts_ipv4_destination(&self, dst: Ipv4Address) -> bool {
        dst == self.common.ipv4_addr() || dst == Ipv4Address::BROADCAST
    }

    pub(super) fn build_arp_request(&self, target_ip: Ipv4Address) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: to_smoltcp_ether(self.ether_addr),
            source_protocol_addr: to_smoltcp_ipv4(self.common.ipv4_addr()),
            target_hardware_addr: SmoltcpEthernetAddress::BROADCAST,
            target_protocol_addr: to_smoltcp_ipv4(target_ip),
        };
        build_arp_frame(&repr)
    }

    pub(super) fn build_arp_reply(
        &self,
        target_ip: Ipv4Address,
        target_mac: EthernetAddress,
    ) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: to_smoltcp_ether(self.ether_addr),
            source_protocol_addr: to_smoltcp_ipv4(self.common.ipv4_addr()),
            target_hardware_addr: to_smoltcp_ether(target_mac),
            target_protocol_addr: to_smoltcp_ipv4(target_ip),
        };
        build_arp_frame(&repr)
    }

    pub(super) fn transmit_frame(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult {
        match self.netdev.ops.transmit(frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Continue { .. } => {
                self.stats
                    .tx_bytes
                    .fetch_add(frame.len() as u64, Ordering::Relaxed);
                self.stats.tx_packets.fetch_add(1, Ordering::Relaxed);
                PacketTxResult::Accepted {
                    frame_len: frame.len(),
                }
            }
            StepOutcome::Yield { .. } => PacketTxResult::Busy,
            StepOutcome::Err(errno) => {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                PacketTxResult::Failed { errno }
            }
        }
    }
}

// ===== IPv6 V2: dynamic NDP — neighbour resolution, mirror of the ARP path =====
impl EtherIface {
    /// Side-effect peek (mirror of `maybe_reply_icmpv4`): learn NS/NA neighbours
    /// and answer solicitations that target one of our addresses. The dispatch
    /// is unchanged and still flows to raw ICMPv6 sockets.
    pub(super) fn maybe_process_ndisc(
        &self,
        dispatch: &PacketDispatch,
        now: Instant,
        guard: Option<&Guard<'_>>,
    ) {
        let PacketDispatch::Icmp6(packet) = dispatch else {
            return;
        };
        // NDP is ICMPv6: parse + verify the checksum against the carried src/dst.
        // The RawIpv6Packet dropped the hop-limit, so the RFC 4861 "hop == 255"
        // off-link guard is not re-checked here (acceptable in the closed slirp
        // env; the real wire NDP path can reinstate it).
        let icmp = match Icmpv6Packet::new_checked(packet.payload.as_slice()) {
            Ok(icmp) => icmp,
            Err(_) => return,
        };
        let repr = match Icmpv6Repr::parse(
            &to_smoltcp_ipv6(packet.src),
            &to_smoltcp_ipv6(packet.dst),
            &icmp,
            &ChecksumCapabilities::default(),
        ) {
            Ok(repr) => repr,
            Err(_) => return,
        };

        match repr {
            Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit { target_addr, lladdr }) => {
                let src = packet.src;
                let src_mac = lladdr.and_then(ether_from_lladdr);
                // Learn the solicitor's mapping from its source link-layer option
                // (skip DAD probes, which source from the unspecified address).
                if let Some(mac) = src_mac {
                    if src != Ipv6Address::UNSPECIFIED {
                        self.learn_ndisc(src, mac, now);
                    }
                }
                // Answer a solicitation that targets one of our addresses.
                let target = from_smoltcp_ipv6(target_addr);
                if self.common.ipv6_addr() == Some(target) {
                    if let (Some(guard), Some(mac)) = (guard, src_mac) {
                        let frame = self.build_neighbor_advert(src, mac, target);
                        let _ = self.transmit_frame(&frame, guard);
                    }
                }
            }
            Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                target_addr, lladdr, ..
            }) => {
                // Learn the advertised target's mapping from its target lladdr.
                if let Some(mac) = lladdr.and_then(ether_from_lladdr) {
                    self.learn_ndisc(from_smoltcp_ipv6(target_addr), mac, now);
                }
            }
            _ => {}
        }
    }

    pub(super) fn learn_ndisc(&self, ip: Ipv6Address, mac: EthernetAddress, now: Instant) {
        self.ndisc_table.lock().insert(
            ip,
            NdiscEntry {
                mac,
                expires_at: now + NDISC_CACHE_TTL,
            },
        );
        self.pending_ndisc.lock().remove(&ip);
    }

    pub(super) fn lookup_ndisc_entry(&self, ip: Ipv6Address, now: Instant) -> Option<NdiscEntry> {
        let mut table = self.ndisc_table.lock();
        match table.get(&ip).copied() {
            Some(entry) if entry.expires_at > now => Some(entry),
            Some(_) => {
                table.remove(&ip);
                None
            }
            None => None,
        }
    }

    pub(super) fn queue_pending_ndisc(&self, ip: Ipv6Address, now: Instant) -> NdiscResolution {
        let mut pending = self.pending_ndisc.lock();
        match pending.get(&ip).copied() {
            Some(NdiscPendingEntry {
                last_error: Some(errno),
                ..
            }) => NdiscResolution::Failed { errno },
            Some(_) => NdiscResolution::Pending { next_hop: ip },
            None => {
                pending.insert(
                    ip,
                    NdiscPendingEntry {
                        addr: ip,
                        attempts: 0,
                        next_probe_at: now,
                        last_error: None,
                    },
                );
                NdiscResolution::Pending { next_hop: ip }
            }
        }
    }

    pub(super) fn ready_pending_ndisc(&self, now: Instant, budget: usize) -> Vec<Ipv6Address> {
        self.pending_ndisc
            .lock()
            .iter()
            .filter_map(|(ip, entry)| {
                (entry.last_error.is_none() && entry.next_probe_at <= now).then_some(*ip)
            })
            .take(budget)
            .collect()
    }

    pub(super) fn pending_entry_for_ndisc_probe(
        &self,
        ip: Ipv6Address,
        now: Instant,
    ) -> Option<NdiscPendingEntry> {
        let mut pending = self.pending_ndisc.lock();
        let entry = pending.get_mut(&ip)?;
        if entry.last_error.is_some() || entry.next_probe_at > now {
            return None;
        }
        if entry.attempts >= NDISC_SOLICIT_RETRY_LIMIT {
            entry.last_error = Some(Errno::EADDRNOTAVAIL);
            return None;
        }
        Some(*entry)
    }

    pub(super) fn mark_ndisc_probe_sent(&self, ip: Ipv6Address, now: Instant) {
        if let Some(entry) = self.pending_ndisc.lock().get_mut(&ip) {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.next_probe_at = now + NDISC_SOLICIT_RETRY_DELAY;
        }
    }

    pub fn pending_ndisc_entry(&self, ip: Ipv6Address) -> Option<NdiscPendingEntry> {
        self.pending_ndisc.lock().get(&ip).copied()
    }

    pub(super) fn build_neighbor_solicit(&self, target: Ipv6Address) -> Vec<u8> {
        let dst_ip = solicited_node_multicast(target);
        let dst_mac = multicast_mac_for(dst_ip);
        let src_ip = self.common.ipv6_addr().unwrap_or(Ipv6Address::UNSPECIFIED);
        let repr = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
            target_addr: to_smoltcp_ipv6(target),
            lladdr: Some(to_smoltcp_ether(self.ether_addr).into()),
        });
        self.build_ndisc_frame(src_ip, dst_ip, dst_mac, repr)
    }

    pub(super) fn build_neighbor_advert(
        &self,
        to_ip: Ipv6Address,
        to_mac: EthernetAddress,
        target: Ipv6Address,
    ) -> Vec<u8> {
        let src_ip = self.common.ipv6_addr().unwrap_or(Ipv6Address::UNSPECIFIED);
        let repr = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
            target_addr: to_smoltcp_ipv6(target),
            lladdr: Some(to_smoltcp_ether(self.ether_addr).into()),
        });
        self.build_ndisc_frame(src_ip, to_ip, to_mac, repr)
    }

    /// Emit an ICMPv6 NDP message, wrap it in an IPv6 header (hop limit 255 per
    /// RFC 4861), then in an Ethernet frame. Mirror of `build_arp_frame`.
    fn build_ndisc_frame(
        &self,
        src_ip: Ipv6Address,
        dst_ip: Ipv6Address,
        dst_mac: EthernetAddress,
        icmp_repr: Icmpv6Repr,
    ) -> Vec<u8> {
        let checksum = ChecksumCapabilities::default();
        let mut icmp_bytes = vec![0u8; icmp_repr.buffer_len()];
        let mut icmp_packet = Icmpv6Packet::new_unchecked(&mut icmp_bytes);
        icmp_repr.emit(
            &to_smoltcp_ipv6(src_ip),
            &to_smoltcp_ipv6(dst_ip),
            &mut icmp_packet,
            &checksum,
        );
        let ip_repr = IpRepr::Ipv6(Ipv6Repr {
            src_addr: to_smoltcp_ipv6(src_ip),
            dst_addr: to_smoltcp_ipv6(dst_ip),
            next_header: IpProtocol::Icmpv6,
            payload_len: icmp_bytes.len(),
            hop_limit: 255,
        });
        let ip_header_len = ip_repr.header_len();
        let mut ip_bytes = vec![0u8; ip_header_len + icmp_bytes.len()];
        ip_repr.emit(&mut ip_bytes[..ip_header_len], &checksum);
        ip_bytes[ip_header_len..].copy_from_slice(&icmp_bytes);
        build_ipv6_ethernet_frame(self.ether_addr, dst_mac, &ip_bytes)
    }
}
