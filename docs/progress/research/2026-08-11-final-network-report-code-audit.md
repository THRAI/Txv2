# Final Network Report Code Audit

Date: 2026-08-11

## Question

What can the final-round report accurately claim about the current network
stack's socket field cleanup, network/link boundary, IPv6 data plane,
RISC-V/LoongArch board support, and socket integration with the file model?

## Audit Baseline

- Branch: `feature/portable-net-vf2-dwmac`
- Commit: `85a6fcc00cbd`
- Report source/template:
  `msp/docsr/内核赛初赛报告.md` and
  `msp/docsss/网络模块文档.typ`
- Produced report fragment:
  `msp/docsss/决赛网络功能改进.typ`

## Findings

1. `SocketPayload` now owns exactly one `SocketImpl`, rather than parallel
   optional `raw_*` engines. TCP state that must move atomically is under one
   `TcpInner` lock. UDP uses the smoltcp packet ring as the real queue and no
   longer mirrors datagrams through shadow `VecDeque`s.
   - `crates/tx-subsystems/src/net/structure/payload.rs:123-139,206-223`
   - `crates/tx-subsystems/src/net/protocol/tcp.rs:44-76`
   - `crates/tx-subsystems/src/net/protocol/udp.rs:41-68`

   The report now also records the TCP layout history. Before `92862a45`
   (2026-07-03), `RawTcpSocket` held separate `socket`, `protocol_state`, and
   `corked_tx` locks. That commit introduced the single-lock `TcpInner` to make
   the availability/cork/send/clear transaction atomic. `connect_attempt` was
   added by `64b48af6` (2026-07-30): its generation plus endpoint tuple lets
   the transport return the exact asynchronous connect attempt that became
   established, failed, or timed out, so a stale event cannot mutate a later
   attempt. `pending_immediate_reply` was added by `c6b5f5f1` (2026-08-06): it
   retains the immediate ACK returned by `smoltcp::Socket::process` until the
   packet sink accepts it. The latter closed the VF2 Git-clone hang where an
   out-of-order ACK was discarded after smoltcp had already advanced its ACK
   state, leaving ordinary dispatch unable to reconstruct the packet.
   - `crates/tx-subsystems/src/net/protocol/tcp.rs:44-75,238-248,504-610`
   - `crates/tx-subsystems/src/net/structure/payload.rs:35-40`
   - `docs/progress/STATUS.md:738-752`
2. `PacketSource`/`PacketTxSink` separate IP-packet production and consumption
   from `NetDeviceOps`, while `EtherIface` owns Ethernet framing and ARP/NDP.
   The report listing now comments each displayed method in place: ingress
   produces a protocol-demultiplexed `PacketDispatch`, packet egress accepts a
   complete IP packet, and device operations consume or produce link-layer
   frames plus MAC/MTU metadata.
   - `crates/tx-subsystems/src/net/packet/mod.rs:17-68`
   - `crates/tx-subsystems/src/net/device.rs:73-125`
   - `crates/tx-subsystems/src/net/protocol/ether/mod.rs:818-835`
3. IPv6 is not loopback-only. Ethernet RX demuxes IPv6 TCP/UDP/ICMPv6; TX has
   IPv6 route selection, NDP, Ethernet framing, and the production netdevice
   path. Recorded QEMU witnesses include external TCP HTTP, UDP echo, and
   `ping6`. RA/SLAAC, DHCPv6, and IPv6 fragmentation/reassembly remain outside
   the implemented surface.
   - `crates/tx-subsystems/src/net/protocol/ether/mod.rs:330-350,696-739,877-915`
   - `crates/tx-subsystems/src/net/protocol/ether/link.rs:254-321`
   - `docs/progress/STATUS.md:1360-1420,1588-1645`
4. The report now emphasizes user-visible RISC-V/LoongArch network functions
   instead of board binding, MMIO, IRQ, DMA, or descriptor details. The shared
   functional surface includes ICMP ping, DNS, HTTP/HTTPS, and Git clone,
   push, and pull. Dual-architecture RV64/LA64 verification completed the
   authenticated clone/push/pull workflow. Real VisionFive 2 and Loongson
   2K1000 evidence covers public ping, HTTPS/TLS, and GitHub clone; the report
   deliberately does not call board push/pull an executed acceptance witness,
   because that final step requires user-owned credentials and remote edits.
   - `docs/progress/STATUS.md:738-776,988-1001,1726-1743`
   - `msp/debug-logs/2026-08-04-vf2-git-clone-operation-ledger.md:61-72`
   - `docs/progress/handoffs/2026-08-06-la2k1000-real-board-git.json:31-35,393-395`
5. "File socket" must be described as integration with the common
   `OpenFile`/fd/VFS I/O model, not as a synonym for `AF_UNIX`. Socket-backed
   anonymous `RNode`s hold `StructPayload::Socket`; `Cap<SocketIdentity>`
   implements `FileOps` for read/write/last-close and participates in common fd
   readiness.
   - `crates/tx-subsystems/src/net/execution/step_socket_open_file.rs:79-112`
   - `crates/tx-subsystems/src/net/file_ops.rs:27-64`
   - `crates/tx-subsystems/src/vfs/fd_ready.rs:342-360`

## Report Placement Note

The editable Markdown source currently numbers the hardware-abstraction chapter
as Chapter 10 and the network-stack chapter as Chapter 11. The fragment is
therefore written as a network-chapter section (`== 决赛功能改进`) and should be
included at the end of the network chapter. Generated
`msp/docsr/content/报告正文.typ` and `msp/docsr/报告-web.typ` must not be edited
directly.

## Verification

- CodeGraph plus targeted source reads were used to trace the live paths.
- A temporary wrapper using the existing network-document A4 typography
  compiled the Typst fragment successfully with `typst compile`.
- `pdfinfo` reported 4 A4 pages.
- A direct-compile visual audit found that the first fragment revision fell
  back to Korean CJK fonts (`HYYeYXSJ`, `UnDotum`, and `UnPen`). The fragment
  now declares the same families and sizes as the network template: 11.5 pt
  Times New Roman/Noto Serif CJK SC body, Noto Sans CJK SC headings, and
  9.5 pt DejaVu Sans Mono/Noto Sans CJK SC code. Recompiled page images have no
  garbled CJK glyphs; `pdffonts` shows only the intended font families.
- The TCP subsection was expanded with the current `TcpInner` source shape,
  the former three-lock layout, and the exact roles and introduction commits
  of `connect_attempt` and `pending_immediate_reply`. The raw code block also
  carries a short Chinese comment on every `TcpInner` field (and on the three
  visible `RawTcpSocket` fields), so the field responsibilities are readable
  without leaving the listing.
- The former board-driver implementation subsection was replaced with a short
  functional summary and representative `ping`/Git commands. It separates the
  dual-architecture clone/push/pull result from the real-board ping/HTTPS/clone
  evidence so the report does not overstate credential-dependent board tests.
- All five listings now use plain fenced Typst raw blocks (`rust` or `sh`). The
  `code-figure` helper, captions, and automatic "code" numbering were removed
  so listings follow the report author's requested source style directly.
- `git diff --check -- msp/docsss/决赛网络功能改进.typ` passed.
- `cargo xtask progress validate` remains blocked by the untouched
  `docs/progress/plans/2026-07-24-network-time-integration.json` legacy
  `completed` status.
- `cargo xtask lint docs` reported the existing 31 broken links and 6
  stale-vocabulary warnings; none points to this audit note or Typst fragment.

## Next Step And Blockers

- Next: author review, then include the fragment at the end of
  `msp/docsss/网络模块文档.typ` or transplant it into the final report's
  chosen source pipeline.
- Blockers: none. The only pending choice is which report source the author
  wants to treat as the final publication source. Repository-wide progress and
  docs validation retain the unrelated baseline failures named above.
