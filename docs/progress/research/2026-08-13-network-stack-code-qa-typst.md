# Network stack code Q&A Typst ledger (2026-08-13)

> Update (2026-08-14): the original 9-page Q001-Q004 ledger was expanded to an
> 18-page Q001-Q022 network judge-style question bank. The new research record
> is `docs/progress/research/2026-08-14-network-judge-question-bank.md`; page
> counts and visual checks below describe the 2026-08-13 edition.

## Scope

This pass answered the first four network-stack research questions from current
code and Git history, then organized the evidence in a reusable Typst document.
The questions cover architecture and the smoltcp boundary, preliminary-to-final
optimization, listener-table structure, and representative debugging cases.

Primary artifacts:

- `msp/docsss/网络栈代码研究问答.typ`
- `msp/docsss/网络栈代码研究问答.pdf`

## Findings

- The current code is best described as six responsibility layers: Linux
  syscall/fd ABI, socket facade/execution/object/index, smoltcp protocol wrapper,
  Ethernet/netns data plane, `NetDevice`, and driver/IRQ/HAL. smoltcp is a
  vendored protocol engine for the enabled TCP/UDP and IPv4/IPv6/Ethernet
  features; it is not the whole txKernel network stack.
- Production receive/transmit integration goes through namespace packet sources,
  `EtherIface`, and packet sinks. `SmoltcpAdapter` is a thin compatibility helper
  used mostly by tests, so the adaptation boundary is broader than one file.
- The vendored fork contains txKernel-side changes for UDP send peeking and byte
  accounting, TCP retransmit priority, receiver-window/SWS behavior, and FIN
  observation. The repository does not preserve the upstream source commit for
  the initial Asterinas-style import, so the full pre-vendor delta is unknown.
- Per the user-provided competition boundary, the preliminary baseline is the
  last `main` commit before 2026-06-15 00:00 +08:00: `09bbbad7`, committed at
  2026-06-14 16:10:18 +08:00. The next `main` commit is `145d2fa7` at
  2026-06-15 12:41:27 +08:00. `1ba1dc28` (2026-07-30 final concurrent
  network/user-I/O work) remains the final representative anchor on `main`.
  The final/current path removes TCP/UDP shadow queues, consolidates locking
  and protocol state, derives readiness from live state, bounds ring memory,
  batches TCP dispatch, improves loopback fairness, and tightens
  listener/conntrack lifecycle bounds.
- The previous draft incorrectly used `fafc4065` (2026-06-29 preliminary
  materials) as the preliminary date anchor. The network subsystem, socket I/O
  shims, and vendored smoltcp have an empty diff between `09bbbad7` and
  `fafc4065`; the historical TCP, UDP, and listener-table file SHA-256 values
  also match. Therefore the code-shape conclusions remain valid, but the date
  and commit attribution have been corrected.
- The current listener table is a fixed-capacity 128-slot `Index` under a
  spinlock. Lookup is a bounded linear scan, including exact/wildcard/dual-stack
  candidates; it is not a hash-table O(1) lookup. A historical `scan_limit`
  active-prefix optimization existed briefly and was later removed.
- The debugging workflow should classify the semantic owner before changing the
  network path: preserve serial/pcap/counter evidence, locate the wake/IRQ or ABI
  boundary, apply the smallest general fix, and rerun both the focused witness
  and broader network gates. The Typst ledger records backlog, readiness,
  zero-length UDP, RV64/LA64 IRQ, EBR, and signal-ABI examples.

## Verification

- Used the repository CodeGraph index first, followed by targeted current-source
  and Git-history inspection.
- Verified the `main` cutoff with `git log main --before`, checked ancestry to
  `1ba1dc28`, compared the relevant path range, and independently hashed the
  historical TCP, UDP, and listener-table sources at both the corrected and old
  anchors.
- `typst compile msp/docsss/网络栈代码研究问答.typ
  msp/docsss/网络栈代码研究问答.pdf` passed with Typst 0.14.2.
- `pdfinfo` reports 9 A4 pages; `pdffonts` reports embedded Unicode-capable CJK,
  Times New Roman, and monospace fonts.
- Rendered pages 1, 5, 6, and 9 to PNG and visually checked the cover/table of
  contents, the corrected cutoff evidence, both halves of the widest
  phase-comparison table, and the final anchor table; no clipping or table
  overflow was observed.
- This was a research/documentation pass. No kernel runtime or QEMU gate was run,
  and no performance delta is claimed without a controlled same-environment A/B.

## Next step and blockers

Append later questions as `Q005`, `Q006`, and so on using the evidence template
already included in the Typst source. If a numeric preliminary-to-final speedup
is required, first identify exact build artifacts and run a controlled benchmark
matrix on the same target and environment.

Blockers to stronger claims are the missing upstream source commit for the first
smoltcp vendor import and the absence of a controlled `09bbbad7`/`1ba1dc28`
benchmark pair. The preliminary cutoff itself is no longer ambiguous because it
is now defined by the user as `main` before 2026-06-15.
