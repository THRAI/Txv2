use super::*;

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
        dst.is_broadcast() || dst == to_smoltcp_ether(self.ether_addr)
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
