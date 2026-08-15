# Network judge-style question bank (2026-08-14)

## Objective

Infer the judging dimensions used in prior OS competition defenses, translate
those dimensions to the txKernel network stack, and prepare code-checked model
answers. The user-facing response lists questions only; answers, likely
follow-ups, evidence boundaries, and code anchors live in the Typst ledger.

Artifacts:

- `msp/docsss/网络栈代码研究问答.typ`
- `msp/docsss/网络栈代码研究问答.pdf`

## Inferred judging dimensions

The prior questions repeatedly test time/ownership, originality and quantities,
end-to-end understanding, platform differences, mechanism plus measured effect,
hard limits, real applications, debugging attribution, and design tradeoffs.
They often begin with a broad claim and then demand one exact number, boundary,
or implementation path that can falsify a rehearsed answer.

Q005-Q022 therefore cover:

- preliminary-to-final and final-day changes;
- smoltcp ownership, local fork changes, and line-count definitions;
- syscall-to-NIC TX and IRQ-to-blocked-recv RX paths;
- RV64/LA64 QEMU and VF2/Loongson 2K1000 device/IRQ differences;
- the pinned fair network delegate, per-flow budgets, and round-robin fairness;
- listener/index complexity, commit-dependent capacities, MTU, datagram, ring,
  backlog, connection-table, and ephemeral-port limits;
- hardest syscall semantics, real applications, and network-layer decoupling;
- two independent debugging stories, backpressure, TCP close/FIN ownership,
  unsupported surfaces, and the next evidence-led scaling investigation.

## Evidence anchors and quantitative checks

- Competition anchors: preliminary `main@09bbbad7`; final
  `main@1ba1dc28`. Current feature worktree: `c62e8824`. Latest merged `main`:
  `867eb5a6`.
- `09bbbad7..1ba1dc28` contains 38 commits touching the network subsystem and
  49 touching network plus selected socket I/O shims and vendored smoltcp. The
  network-subsystem diff is 62 files and `+7284/-2075`.
- At `1ba1dc28`, `crates/tx-subsystems/src/net` has 108 Rust files and 46,289
  physical lines including tests; production paths excluding `tests/` and
  `tests.rs` have 77 files and 30,033 physical lines. Vendored smoltcp `src/`
  has 94 files and 51,957 physical lines. These are repository ownership and
  physical-line counts, not personal authorship or enabled-feature SLOC.
- Relative to first-vendor anchor `6d01880a`, the final smoltcp fork delta is
  39 insertions in four files; latest `main` is `+195/-26` in six files.
- The feature worktree has 128 listener and 256 generic connection slots.
  Latest `main` keeps 128 listener slots but has 4096 TCP-bound and 4096 TCP
  connection slots. Lookup remains fixed-array linear scan.
- Code limits checked directly include 1500 physical virtio MTU, 65,535
  loopback MTU and IPv4 total length, 65,507 maximum IPv4 UDP payload, 64 KiB
  TCP/UDP ring backing, 128 staged `SOMAXCONN`, Linux-style N+1 pending backlog
  semantics, and 64 rotating ephemeral ports `[49152,49216)`.
- Runtime evidence uses the maintained QEMU 44/44 netperf/iperf matrix, latest
  DHCP/Git/IRQ gates, VF2 40/40 concurrent HTTP plus 600/600 ICMP and Git/fsck,
  and Loongson 2K1000 7818-object/17.47 MiB clone plus 1077/1077 ICMP.

## Verification

- Used CodeGraph before targeted source inspection, then checked current files,
  historical blobs, main ancestry, path-specific commit counts/diffs, line
  counts, platform bundles, driver/IRQ paths, and hardware/QEMU ledgers.
- `typst compile msp/docsss/网络栈代码研究问答.typ
  msp/docsss/网络栈代码研究问答.pdf` passed.
- `pdfinfo` reports 18 A4 pages. All 18 pages were rendered; pages 1, 8, 10,
  and 14-18 were visually inspected, including the outline, old/new section
  boundary, long answer blocks, continuation pages, template, and final table.
  No clipping or table overflow was observed.
- This was research/documentation work. No new kernel/QEMU/hardware test was
  run; runtime numbers are cited from preserved logs rather than rerun.

## Next and blockers

Next, use the displayed questions as an oral mock defense: answer each without
opening the PDF, then compare against the model answer and follow-up trap. Add
new user questions at Q023 and later.

Blockers to stronger claims remain the missing upstream source commit for the
first Asterinas smoltcp import and the lack of a controlled same-environment
`09bbbad7`/`1ba1dc28` performance A/B. Physical line counts also cannot prove
personal authorship; any author-attribution question needs a separate blame and
provenance audit with an agreed scope.
