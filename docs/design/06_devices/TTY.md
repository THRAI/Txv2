# TTY Subsystem

<!-- txdoc:06-DEVICES-TTY -->

**Status.** v1 (2026-04-24). Draft.

**Purpose.** Specify the terminal subsystem: hardware serial TTYs, pseudo-terminals, line discipline, job control, and devpts projection. TTY is the one subsystem where a `StructBacked` RNode refers to a genuinely dynamic entity (not a `&'static` table), because ptys are allocated and freed at runtime. This document explains the `TtyIdentity` / `TtyPayload` factoring, the `TtyTransport` discriminator that abstracts over hardware vs pty, the line-discipline state machine, and the session/pgrp bindings that make `SIGHUP` / `SIGTTIN` / `SIGTTOU` work.

**Audience.** Anyone implementing terminal I/O, process groups, signal delivery on controlling terminals, or devpts.

**Companion documents.**

- [`DEVICE.md`](./DEVICE.md) — tier-2 `CharDeviceBinding` consumed by hardware-backed TTYs via `TtyTransport::Hardware`.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) §3, §8.1.1 — Identity/Payload factoring convention. TTY is factored per this convention.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2 — `StructBacked::Tty(Cap<TtyIdentity>)`; §2.3 lifecycle notes. This document **revises** PAGE_BACKED's note that the payload is `TtyData`: TTY now factors per `object_model_v2 §8.1.1`, with `Tty(Cap<TtyIdentity>)` as the identity handle.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — BIF-*, PRED-*, OBL-*, SIG-*. TTY's entity split is a BIF application; session/pgrp bindings use OBL addressability; `hangup_port` is a SIG-4 / SIG-11 attachment.
- [`LIVENESS_v2.1.md`](../00_meta-framework/archived/LIVENESS_v2.1.md) — archived partial-order source material.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — this document contributes catalog entries for TTY wires (`hangup_port`, `input_readable`, `output_writable`, `session_ctl_port`); see §8.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — RawQueue / RawPort used.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — standard four-module layout (§9).
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — step/script model.

### Zone-derived type policy

<!-- txdoc:TTY-ZONE-DERIVED-TYPE-POLICY-1 -->

TTY is the dynamic-device case that uses the full policy-zone interface:

| TTY declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `TtyIdentity` | `Cap<TtyIdentity>`, `Weak<TtyIdentity>`, `IdentRef<'g, TtyIdentity>` | addressable terminal identity retained by RNodes, sessions, and pty peers |
| `TtyPayload` | `PayloadCap<TtyPayload>` reached through `TtyIdentity.payload` | line discipline, queues, and transport state |
| Hardware transport | `&'static CharDeviceBinding` | static tier-2 device fact, no zone |
| Pty peer relation | `Cap<TtyIdentity>` or guarded witness to peer identity | addressability, not payload ownership |
| Controlling-terminal binding | `Cap<Session>` and `Cap<ProcessGroup>` | process-owned identity evidence |
| devpts projection rows | projection entries over live TTY identities | EBR-observed/revalidated view |

TTY code names these role-shaped handles. The entity-zone declarations choose
the hidden retained/EBR policy; ioctl, read/write, hangup, and pty allocation
steps do not pass policy parameters.

---

## 1. Motivation

<!-- txdoc:TTY-MOTIVATION-1 -->

A tty is three concerns superimposed on one fd:

1. **Terminal abstraction.** Line discipline, termios, cooked/raw mode, input and output queues, window size, a session-and-pgrp binding. Pure kernel state, no hardware required.
2. **Terminal endpoint.** The specific "this tty." For a hardware tty: bound to a specific UART. For a pty: bound to its master/slave twin.
3. **Byte transport.** For hardware: a driver pushing bytes to/from MMIO plus IRQ. For pty: a ring between master and slave, no hardware.

Linux collapses (1) and (2) into `struct tty_struct` and delegates (3) to `struct tty_driver` / `tty_operations`, injecting fops per-open. PAGE_BACKED has already retired open-time fops injection. So TTY's factoring must be explicit.

TTY's central decision: **the tty subsystem is layered on top of the char-device subsystem, not equal to it.** A hardware tty is a tty with a `CharDeviceBinding` transport; a pty is a tty with a peer-tty transport. The char-device driver (e.g., `ns16550a`) sees only bytes in and bytes out; it does not know about termios or line discipline or pgrps. The tty layer adds the cooked semantics.

---

## 2. Factoring

<!-- txdoc:TTY-FACTORING-1 -->

### 2.1 Why TTY must split

<!-- txdoc:TTY-WHY-TTY-MUST-SPLIT-1 -->

Per `object_model §8.1.1`, an entity admits the Identity/Payload factoring when `structural ⟂ payload` holds. TTY does:

- **Hangup** drops payload (line discipline, queues, transport) while identity persists briefly so that subscribers can observe `SIGHUP`, `poll` returns `POLLHUP`, and any session still holding this tty as its controlling terminal (via the per-process `controlling_tty: Option<Cap<TtyIdentity>>` pointer in its session leader) retains an addressable handle until the session cleans up.
- **Session / pgrp bindings** target the tty across hangup — a process group's "controlling tty" pointer must name the same tty it was originally attached to, even if the tty has hung up, so that signal delivery routes correctly (and fails cleanly with no-such-tty errors, not with silent misdelivery to a replacement).
- **Pty close ordering** can drop the payload (queues drained, peer EOF'd) before all fds on the slave close; remaining fd holders see EIO on ops but the identity keeps their RNode meaningful.

Therefore:

- `TtyIdentity` — name, kind, session/pgrp binding, wires. Persists through hangup.
- `TtyPayload` — termios, ldisc state, queues, transport. Dropped on hangup or final close.

### 2.2 Identity

<!-- txdoc:TTY-IDENTITY-1 -->

```rust
// frame/tty/structure/identity.rs
pub struct TtyIdentity {
    meta: SlotMeta,

    pub kind: TtyKind,
    pub index: u32,                  // ttyS<N>, pts/<N>, console_major=0
    pub name: FixedName<16>,         // e.g. "ttyS0", "pts/3", "console"

    /// Controlling-terminal binding: the canonical session for which this tty
    /// is controlling, and the canonical foreground process group within that
    /// session. `None` until TIOCSCTTY is used.
    ///
    /// **Concurrency discipline (see §2.5):** published-value pattern.
    /// Readers (every ldisc signal dispatch, SIGTTIN/TTOU check, poll) take
    /// a lock-free epoch-guarded snapshot. Writers (TIOCSCTTY, TIOCSPGRP,
    /// TIOCNOTTY, session-leader exit, hangup) publish via `swap_commit`
    /// (CONCEPTS §8.6) against the AtomicSlot, with the appropriate
    /// identity-layer precondition checked atomically at commit.
    pub session_pgrp: AtomicSlot<Option<SessionPgrp>>,

    // --- wires (hosted on Identity per BIF-5 and §2.4) ---
    pub input_readable:   RawQueue,  // POLLIN
    pub output_writable:  RawQueue,  // POLLOUT
    pub hangup_port:      RawPort,   // carrier-drop / master-close
    pub session_ctl_port: RawPort,   // session/pgrp changes

    // Payload-capable retention
    pub payload: Option<PayloadCap<TtyPayload>>,
}

pub enum TtyKind {
    SerialHardware,     // /dev/ttyS*, wraps a CharDeviceBinding
    PtyMaster,          // /dev/ptmx-allocated
    PtySlave,           // /dev/pts/<N>
    Console,            // /dev/console — alias to a SerialHardware
}

/// Session-and-pgrp binding on a controlling terminal. PROCESS owns
/// `Session` and `ProcessGroup` as canonical identity-only entities; pid
/// namespace numbers are only view signifiers for syscalls and projections.
///
/// Cohort dispatch (SIGHUP to session, SIGINT/SIGTSTP to fg pgrp) uses these
/// canonical identities directly. Any user-supplied sid/pgid is resolved
/// through the caller's pid namespace before this binding is changed.
pub struct SessionPgrp {
    pub session: Cap<Session>,
    pub foreground_pgrp: Cap<ProcessGroup>,
}
```

### 2.3 Payload

<!-- txdoc:TTY-PAYLOAD-1 -->

```rust
// frame/tty/structure/payload.rs
pub struct TtyPayload {
    meta: SlotMeta,

    /// Termios: read by every input byte and every output byte of
    /// every write; mutated rarely by `tcsetattr`.
    ///
    /// **Concurrency discipline (see §2.5):** published-value pattern.
    /// Writers build a new `Termios`, publish the new `Arc<Termios>`
    /// via `swap_commit` against this AtomicSlot. Readers take
    /// an epoch-guarded snapshot, hold the resulting `Arc<Termios>`
    /// for the duration of one byte's processing, drop. A single
    /// input-byte pass sees one coherent Termios; a concurrent
    /// `tcsetattr` becomes visible on the *next* byte. No RwLock.
    pub termios: AtomicSlot<Arc<Termios>>,

    /// Window size: 4 u16s packed into a u64 (`ws_row`, `ws_col`,
    /// `ws_xpixel`, `ws_ypixel`). Read by TIOCGWINSZ, written by
    /// TIOCSWINSZ (which also fires SIGWINCH to the fg pgrp).
    ///
    /// **Concurrency discipline (see §2.5):** packed-atomic pattern.
    /// Reads and writes are single atomic operations; no coordination
    /// primitive needed beyond the atomic itself. SIGWINCH dispatch
    /// happens at the publish sub-phase of `step_ioctl_tiocswinsz`,
    /// paired with the `store` per SIG-4.
    pub window_size: AtomicU64,

    /// Line-discipline state: cooked-mode buffer, column counter,
    /// flow-stopped flag, `lnext` escape. See §3 for contents.
    ///
    /// **Concurrency discipline (see §2.5):** single-writer pattern,
    /// no lock. The ingest step (`tty::step_ingest_commit`) is the
    /// sole writer under normal byte flow; per-tty there is exactly
    /// one ingest step in flight at a time, enforced by the reactor's
    /// single-task-per-channel scheduling for hardware ttys and by
    /// the pty peer's single-writer-by-construction for ptys.
    /// External mutators (`tcsetattr`'s `on_termios_changed` flush
    /// when ICANON turns off mid-line) route through the
    /// **ingest linearizer** — see §3.5 — which ensures the flush
    /// happens as part of an ingest-step commit, not concurrently.
    /// The `UnsafeCell` wrapper encodes "only the linearizer-holder
    /// may dereference mutably"; all access points are audited.
    pub ldisc_state: UnsafeCell<LdiscState>,

    /// Input ring: bytes awaiting user `read()`.
    ///
    /// **Concurrency discipline (see §2.5):**
    /// - Hardware transport: **SPMC**. One producer (ingest step, which
    ///   also holds the ldisc linearizer), multiple consumers (POSIX
    ///   permits concurrent `read()` calls on the same fd; each
    ///   acquires some bytes). Mutex serializes consumer access;
    ///   producer contends only with consumers, not with itself.
    /// - Pty transport: **MPMC**. Multiple producers (concurrent
    ///   `write()` calls on the peer fd, each running its own
    ///   `step_write_commit`) and multiple consumers. Mutex
    ///   serializes both sides. POSIX guarantees `write()` atomicity
    ///   only up to `PIPE_BUF`; txKernel honors this by holding the
    ///   mutex for the duration of a single `write()` call's push,
    ///   so writes ≤ PIPE_BUF are atomic relative to other writes.
    pub input_queue: Mutex<TtyRing>,

    /// Output ring: bytes produced by `write()`, awaiting transport drain.
    ///
    /// **Concurrency discipline (see §2.5): MPSC.** Multiple
    /// producers (concurrent `write()` calls from user threads), one
    /// consumer (the transport-drain step — hardware: IRQ-scheduled
    /// driver TX drain; pty: peer's ingest linearizer). Mutex
    /// serializes producer push. Same PIPE_BUF atomicity guarantee
    /// as `input_queue`.
    ///
    /// **Note on UART TX serialization:** the hardware-side
    /// serialization of UART register writes is driver-internal and
    /// lives inside `CharDeviceBinding.driver_state`, not here.
    /// This queue's discipline only governs user-visible byte
    /// serialization between tty and driver.
    pub output_queue: Mutex<TtyRing>,

    pub transport: TtyTransport,
}

pub enum TtyTransport {
    /// Hardware-backed: an `&'static CharDeviceBinding` whose driver
    /// implements raw byte I/O. Ops on the tty ultimately invoke the
    /// char-device driver's ops via this field.
    Hardware {
        binding: &'static CharDeviceBinding,
    },

    /// Pty-backed: the peer twin. Master's transport points to slave's
    /// TtyIdentity and vice-versa. I/O is ring-to-ring between the two
    /// payloads; no driver, no hardware.
    Pty {
        peer: Cap<TtyIdentity>,
    },
}
```

### 2.4 Why wires live on Identity

<!-- txdoc:TTY-WHY-WIRES-LIVE-IDENTITY-1 -->

Wires (input_readable, output_writable, hangup_port, session_ctl_port) live on `TtyIdentity` because:

- **SIG-4 publication-ordering**: `hangup_port` must fire from `step_hangup_commit`, which drops `TtyPayload`. If the wire were on payload, it would be unreachable after payload drop — subscribers subscribed before hangup would be woken by a wire that no longer exists. Identity survives; so do the wires.
- **BIF-5 single-carrier**: each wire names exactly one retention domain. With wires on Identity, the retention domain is the Identity slot — clean.
- **Subscriber stability**: poll/epoll registrations on a tty fd subscribe to wires on Identity; the subscription survives hangup, fires `POLLHUP`, and remains valid until the subscriber unsubscribes.

### 2.5 LIVENESS entries

<!-- txdoc:TTY-LIVENESS-ENTRIES-1 -->

Two projection rows owned by this subsystem:

**TtyIdentity:**

```
structural ⇐ addressability
              — any session leader holding this tty as controlling
                carries a Cap<TtyIdentity> in its controlling_tty
                field on ProcessIdentity, implying identity retention.
                Additionally, every open fd on a devfs RNode for this
                tty carries a Cap<TtyIdentity> through
                StructBacked::Tty(Cap<TtyIdentity>).
```

**TtyPayload:**

```
structural ⇐ payload
              — operational evidence (OpenPin from OpenFiles) pins
                TtyPayload through Identity.payload.
```

**Split in operation:**

```
TtyIdentity.addressability ⟂ TtyPayload.payload
              — post-hangup: addressability true (any session with
                this tty as controlling still carries a Cap on
                TtyIdentity via its controlling_tty field), payload
                false (queues, ldisc_state, transport all gone).
```

---

### 2.6 Intra-TtyPayload concurrency discipline

<!-- txdoc:TTY-INTRA-TTYPAYLOAD-CONCURRENCY-DISCIPLINE-1 -->

Every shared field on `TtyIdentity` and `TtyPayload` carries an explicit concurrency contract chosen to match its access pattern. This section names the contracts and their justifications. The underlying framework rules come from [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) and [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) publication and binding rules.

**Two disciplines coexist in TTY.** (a) The standard IPC-producer-serialization pattern for `input_queue` / `output_queue` — identical in shape to what pipe, socket send/recv buffers, and POSIX message queues use for concurrent `read()` / `write()` on the same fd. Not a TTY invention; TTY inherits it. (b) A transformation-layer single-writer pattern for `ldisc_state`, specific to subsystems with a kernel-internal transformation step between producer and consumer queues. TTY is the first subsystem with this shape; whether the pattern recurs elsewhere will decide whether it becomes a shared primitive.

**TTY-1 (no unnamed locks).** No field in `TtyIdentity` or `TtyPayload` uses `Mutex<T>` or `RwLock<T>` as a generic shared-mutable-state wrapper. Every lock-bearing field names the access pattern (SPSC / SPMC / MPSC / MPMC / single-writer / published-value / packed-atomic) that justifies its coordination primitive. Reviewers rejecting a `Mutex<T>` should check whether an atomic-slot, packed-atomic, single-writer-linearizer, or lock-free MPSC structure is the correct primitive for the actual pattern; reach for a mutex only when the pattern is genuinely multi-party on both sides.

**Patterns used in TTY.**

| Pattern | Primitive | Fields |
|---|---|---|
| Published-value (read-many, write-rare, whole-value replace) | `AtomicSlot<Arc<T>>` | `TtyPayload::termios` |
| Published-slot (read-many, write-rare, Option of small record) | `AtomicSlot<Option<T>>` with `swap_commit` discipline | `TtyIdentity::session_pgrp` |
| Packed-atomic (small fixed-size value, read/write by syscall) | `AtomicU64` (packed fields) | `TtyPayload::window_size` |
| Single-writer with external-mutator linearizer | `UnsafeCell<T>` with linearizer access discipline (§3.5) | `TtyPayload::ldisc_state` |
| SPMC (one producer, many consumers) | `Mutex<TtyRing>` | `TtyPayload::input_queue` on hardware transport |
| MPMC (many producers, many consumers) | `Mutex<TtyRing>` | `TtyPayload::input_queue` on pty transport |
| MPSC (many producers, one consumer) | `Mutex<TtyRing>` | `TtyPayload::output_queue` |

**Why the ring fields use `Mutex` — the IPC-producer-serialization pattern.** TTY's `input_queue` and `output_queue` serialize concurrent user-thread writes and reads in the same way every other IPC-shaped kernel primitive does: **mutex on the ring, held for the duration of a single producer or consumer operation.** POSIX-family semantics across pipe, socket send/recv buffers, and POSIX message queues all require this: `write()` (or `send()`, `mq_send()`) is atomic up to a subsystem-specific bound (`PIPE_BUF` for pipe-family, per-datagram for `SOCK_DGRAM`, per-message for mq), and the atomicity follows directly from holding the ring mutex for one full push. TTY inherits this pattern; the mutex is not a TTY invention and is not lazy — it is the canonical answer to the multi-producer ring question, applied to the TTY use case.

Per-pattern specifics for TTY:

- Hardware-transport `input_queue`: **SPMC**. Single producer (the ingest step — see below for why ingest is single-writer). Multiple consumers (POSIX permits concurrent `read()` on the same fd). Mutex serializes the consumer side; the producer side is contention-free against other producers but must acquire the mutex to avoid racing with consumers. Atomicity bound: consumers see coherent line-granular (ICANON) or byte-granular (raw) reads.
- Pty-transport `input_queue`: **MPMC**. Multiple producers (concurrent `write()` on the peer fd, each running its own `step_write_commit` and pushing post-ldisc bytes to this side's `input_queue`); multiple consumers (concurrent `read()` on this side). Mutex serializes both ends. Atomicity bound: de facto `PIPE_BUF`, matching Linux's tty implementation.
- `output_queue`: **MPSC**. Multiple producers (concurrent user `write()` calls); one consumer (transport drain — driver TX for hardware, peer ingest linearizer for pty). Mutex serializes producers.

Lock-free SPMC/MPSC/MPMC rings exist and may replace the mutex as an optimization without changing the discipline. When PIPE.md or SOCKET.md are written, their producer-serialization paragraphs will look textually similar; at the point where three subsystems carry the same paragraph, factoring to a shared concepts section becomes worthwhile. Not before.

**Why `ldisc_state` is not lock-bearing.** The ingest step (`tty::step_ingest_commit`) is, by construction, the single authority that mutates `ldisc_state` under byte flow:

- For hardware ttys: the reactor dispatches exactly one ingest task per tty, serialized by the reactor's per-channel queue.
- For ptys: the peer's `step_write_commit` runs the ingest pipeline on this side; concurrent writes on the peer fd are serialized by the peer's `output_queue` mutex before they reach this side's ingest, so at most one ingest pass for this tty is in flight at a time.

The only cross-step mutator is `tcsetattr`'s `on_termios_changed` flush, which needs to touch `cooked_buf` when ICANON turns off mid-line. That mutator routes through the **ingest linearizer** (§3.5) — effectively a ticket enqueued onto the ingest step's work stream — rather than acquiring a lock. This preserves the "exactly one writer" invariant without an explicit mutex and without per-byte lock-acquisition overhead.

**UART TX serialization is not tty-side.** The tty subsystem's `output_queue` serializes user bytes into a ring. The *hardware* TX FIFO serialization — writes to the UART's THR register must be single-threaded or bytes corrupt — is a driver-internal concern that lives inside `CharDeviceBinding.driver_state`, not on `TtyPayload`. The tty hands bytes to the driver via `CharDeviceOps::step_kick_tx(binding, ...)`; the driver internally serializes its TX path (typically: one driver-scheduled TX-drain coroutine per binding, using atomics or a small internal lock on the FIFO-space accounting). This separation lets the tty layer be oblivious to hardware serialization requirements.

**Cross-reference.** The published-value, slot-locked, and substrate-linearized publication mechanisms are the `swap_commit` and related primitives in CONCEPTS §8.6's conditional-commit family. The ingest-linearizer is a TTY-local instance of the single-writer-with-external-mutator pattern; its protocol is specified in §3.5.

---

## 3. Line discipline

<!-- txdoc:TTY-LINE-DISCIPLINE-1 -->

**LDSC-1 (N_TTY only).** txKernel implements exactly one line discipline — the classical Linux `N_TTY`. It is not pluggable. `LdiscState` and the `ldisc_input` / `ldisc_output` functions are hardcoded to N_TTY semantics; there is no discipline registry, no `tty_ldisc_ops` vtable, no `TIOCSETLD` ioctl, and no variant type to match on. Every tty — hardware serial, pty master, pty slave, `/dev/console` — runs the same discipline code. Behavioral differences between use cases (cooked shell vs. raw pty master) are expressed entirely by **termios flags** driving the same code path.

**Rationale.** The target workload (busybox / gcc / nginx / ssh-over-ethernet) uses exactly one discipline. Historical disciplines — `N_PPP`, `N_SLIP`, `N_HDLC`, `N_R3964`, `N_IRDA`, `N_X25`, `N_6PACK` — are either obsolete protocols or handled in userspace on modern systems. Adding a discipline registry costs: a function-pointer vtable, a registry with lifetime rules, `TIOCSETLD`-time state flushing, and per-discipline attach/detach protocols — all for a feature that will not be exercised. The refactor to add pluggability later, should a concrete second discipline materialize, is bounded: turn `LdiscState` into an enum, make `ldisc_input` / `ldisc_output` dispatch on it. This is one-to-two days of work at that future time, as opposed to carrying speculative machinery indefinitely.

**Pty master "raw mode" is not a separate discipline.** On Linux the pty master is effectively raw because it runs a discipline with most flags cleared. In txKernel we achieve the same by initializing pty-master termios with `ICANON`, `ECHO`, `ISIG`, `OPOST`, `IXON`, most `iflag`/`oflag` bits cleared, so that the single N_TTY code path's cooked-mode/signal-generation/flow-control branches are disabled on that endpoint. No variant, no alternative implementation — same code, different termios.

### 3.1 Feature set (N_TTY, v1)

<!-- txdoc:TTY-FEATURE-SET-N-TTY-V1-1 -->

- **Cooked mode** (`ICANON`): line editing with `VERASE` / `VKILL` / `VWERASE` / `VREPRINT` / `VLNEXT`; line buffering until `\n`, `VEOF`, or `MAX_CANON` (255 bytes).
- **Non-canonical mode** (`!ICANON`): immediate byte delivery, `VMIN` / `VTIME` control read completion.
- **Signal generation** (`ISIG`): `c_cc[VINTR]` → SIGINT, `c_cc[VQUIT]` → SIGQUIT, `c_cc[VSUSP]` → SIGTSTP. Delivered to the controlling tty's foreground pgrp via `deliver_posix_signal` (§4, §6).
- **Echo control** (`ECHO`, `ECHOE`, `ECHOK`, `ECHONL`): keystroke echo, including line-edit feedback.
- **Output post-processing** (`OPOST`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`): `\n`/`\r` translation.
- **Flow control** (`IXON`, `IXOFF`, `IXANY`): `c_cc[VSTOP]` (^S) / `c_cc[VSTART]` (^Q).
- **Input-side translation** (`INLCR`, `IGNCR`, `ICRNL`): `\r`/`\n` mapping.
- **Special-char table**: `VEOF`, `VINTR`, `VQUIT`, `VERASE`, `VKILL`, `VSTART`, `VSTOP`, `VSUSP`, `VEOL`, `VMIN`, `VTIME`, `VWERASE`, `VREPRINT`, `VLNEXT`. Layout matches Linux for ABI compatibility.

Deliberately **not implemented** even under N_TTY:

- `XCASE` (obsolete uppercase/lowercase folding for 1970s terminals): accepted at `tcsetattr` for ABI compatibility, silently ignored.
- `TAB1`..`TAB3`, `NL1`, `CR1`..`CR3`, `BS1`, `VT1`, `FF1` output-delay modes: accepted, silently treated as the zero-delay variant.
- UTF-8 multibyte erase (Linux's `IUTF8` behavior): not honored; `VERASE` erases one byte regardless.

These are listed here as settled decisions, not open questions. They are not revisited in §10.

### 3.2 Input-side pipeline

<!-- txdoc:TTY-INPUT-SIDE-PIPELINE-1 -->

Called by `tty::step_ingest_commit` when bytes arrive *from* the transport (hardware UART's reactor-scheduled RX step, or the pty peer's `step_write_commit`). One call per byte or short burst. Pipeline:

```
transport delivers bytes
    → for each byte:
        parity/framing check (IGNPAR / PARMRK)
        input mapping (INLCR, ICRNL, IGNCR)
        signal check (ISIG + c_cc[VINTR|VQUIT|VSUSP])
            → emit LdiscInputEffect::SignalFgPgrp(sig)
        flow-control (IXON + c_cc[VSTOP|VSTART])
            → update LdiscState.flow_stopped
        echo (ECHO + ECHOE/ECHOK/ECHONL for ICANON)
            → push echoed bytes into output_queue (side-effect)
        canonical accumulation (ICANON):
            VERASE / VKILL / VWERASE / VREPRINT / VLNEXT editing
            on newline / VEOF / MAX_CANON:
                flush cooked_buf → input_queue
                → emit LdiscInputEffect::LineCommitted
        non-canonical:
            byte → input_queue directly
            → emit LdiscInputEffect::QueuedForRead
```

The input-side code does **not** post signals itself. When ISIG matches, it returns `LdiscInputEffect::SignalFgPgrp(SIGINT)` to the caller; the tty step consumes this by reading the TtyIdentity's `session_pgrp.foreground_pgrp` and dispatching through `deliver_posix_signal(SignalTarget::ProcessGroup(pgrp), sig, ...)` (see §4, §6). Ldisc does not reference pgrp, session, or process types directly — its only knowledge of job control is that signal delivery is the caller's job.

The commit sub-phase of `tty::step_ingest_commit` is the publication point: all side effects — `input_queue` push, echo to `output_queue`, signal dispatch via `deliver_posix_signal`, and wire fires on `input_readable` — happen together, linearized by the step's commit. Per SIG-4, no wake observer can see a queue state without the corresponding wire fire.

### 3.3 Output-side pipeline

<!-- txdoc:TTY-OUTPUT-SIDE-PIPELINE-1 -->

Called by `tty::step_write_commit` on user `write(tty_fd, buf)`:

```
user write(buf, len)
    → ldisc_output::process_bytes(state, termios, buf, output_queue)
        apply OPOST transformations:
            ONLCR: \n → \r\n
            OCRNL: \r → \n  (when OPOST on)
            ONOCR: suppress \r when column == 0
            ONLRET: \n resets column
        append processed bytes to output_queue
        update state.column
    → kick transport:
        Hardware { binding } → binding.ops.step_kick_tx(binding, ...)
        Pty { peer }         → deliver to peer.input_queue,
                               run peer-side ldisc input pipeline,
                               fire peer.input_readable
    → fire output_writable if queue has space for more
```

The output side is simpler than the input side: no signal generation, no canonical buffering, no echo. OPOST is the only transformation. Output bytes flow one way: user → queue → transport.

For ptys, "kick transport" crosses the transport boundary and runs the peer's input-side pipeline; this is how the shell's output becomes input to the ssh server's read side (or vice versa). The cross-side call is one step, linearized at the writing side's commit.

### 3.4 Ldisc ↔ tty interface

<!-- txdoc:TTY-LDISC-TTY-INTERFACE-1 -->

Three functions, all in `frame/tty/ldisc/`. They are plain functions, not methods on a trait. No `dyn LineDiscipline` exists.

```rust
// frame/tty/ldisc/input.rs
pub fn process_input_byte(
    state: &mut LdiscState,
    termios: &Termios,
    input_queue: &mut TtyRing,
    output_queue: &mut TtyRing,   // echo bytes land here
    byte: u8,
) -> LdiscInputEffect;

// frame/tty/ldisc/output.rs
pub fn process_output(
    state: &mut LdiscState,
    termios: &Termios,
    output_queue: &mut TtyRing,
    bytes: &[u8],
) -> usize;

// frame/tty/ldisc/termios_change.rs
pub fn on_termios_changed(
    state: &mut LdiscState,
    old: &Termios,
    new: &Termios,
    input_queue: &mut TtyRing,
);
```

`LdiscInputEffect` is the discriminated return of `process_input_byte`:

```rust
pub enum LdiscInputEffect {
    /// No user-visible effect (byte was buffered into cooked_buf, or absorbed
    /// by flow control, or was an editing keystroke).
    Absorbed,
    /// Byte was appended to input_queue; caller should fire input_readable.
    QueuedForRead,
    /// A cooked-mode line was committed to input_queue; caller should fire
    /// input_readable. Equivalent to QueuedForRead for the caller's purposes
    /// but distinguished for observability.
    LineCommitted,
    /// ISIG matched a control character. Caller should read the foreground
    /// pgrp binding and dispatch via deliver_posix_signal. Ldisc does not
    /// post the signal itself.
    SignalFgPgrp(Signal),
    /// Flow-control transition (XOFF received or XON received). Caller may
    /// pause or resume transport TX. No signal, no queue change.
    FlowControl(FlowCtl),
}

pub enum FlowCtl { Stop, Start }
```

**`on_termios_changed`** is the escape hatch for state that depends on termios flags. Called at the commit sub-phase of `tty::step_ioctl_tcsetattr_commit`. Current responsibilities:

- If `ICANON` is being turned off and `cooked_buf` is non-empty, flush `cooked_buf` to `input_queue` (POSIX: "you typed it, you'll read it").
- If `ECHO` is being turned off mid-line, discard any pending echo bytes in `output_queue`'s echo region.
- If special-character values change (`c_cc[VINTR]` reassigned), no ldisc-state change — the new values take effect on next byte.

Small surface, one entry per mutable-termios-affected state variable.

**What is deliberately not in the interface.** There is no `open()` / `close()` / `hangup()` ldisc hook. LdiscState is initialized by its constructor (called when TtyPayload is allocated) and dropped when TtyPayload reclaims. Hangup drops TtyPayload; nothing else to do at the ldisc layer.

### 3.5 Ingest linearizer

<!-- txdoc:TTY-INGEST-LINEARIZER-1 -->

The ingest linearizer is the TTY-local coordination primitive that makes the single-writer discipline on `ldisc_state` (§2.6) work across the two kinds of mutators — byte flow from transport, and cross-step termios changes.

**The problem it solves.** `ldisc_state` needs at most one writer at a time, but two step functions plausibly want to mutate it:

- `tty::step_ingest_commit` — normal byte flow; runs the N_TTY input pipeline per byte; mutates `cooked_buf`, `column`, `flow_stopped`, `lnext`.
- `tty::step_ioctl_tcsetattr_commit` — when `ICANON` or `ECHO` changes, the `on_termios_changed` hook must touch `cooked_buf` (flush) or `output_queue`'s echo region. This step is *not* the ingest step and may run on a different thread/coroutine.

Without coordination, these two could race. Wrapping `ldisc_state` in a `Mutex` would solve it but at the cost of per-byte lock acquisition during ingest (hot path). The linearizer achieves the same correctness without the per-byte cost.

**Mechanism.** Each `TtyPayload` carries an additional single-slot channel:

```rust
pub struct TtyPayload {
    // ... fields from §2.3 ...

    /// Ingest linearizer: a single-slot deferred-mutation channel.
    /// Other steps post side-effect closures here; the next ingest
    /// commit drains the slot and applies them atomically with its
    /// own mutations. See §3.5.
    pub ingest_linearizer: AtomicSlot<Option<Box<dyn FnOnce(&mut LdiscState, &mut TtyRing, &mut TtyRing) + Send>>>,
}
```

The slot holds zero or one pending side-effect closure. Two protocols interact:

**Ingest-step protocol (the sole `ldisc_state` writer).** At the start of its commit sub-phase, the ingest step takes the linearizer slot (`swap_commit` with `None`). If a closure is present, it is applied to `&mut ldisc_state`, `&mut input_queue`, `&mut output_queue` as the first operation of the commit. The step then proceeds with byte-flow processing. Exclusive `&mut` access is structurally safe because the ingest step is the *only* step that mutably dereferences the `UnsafeCell<LdiscState>`, and the reactor guarantees at most one ingest task in flight per tty at a time.

**External-mutator protocol (e.g., tcsetattr).** `step_ioctl_tcsetattr_commit` does not touch `ldisc_state` directly. Instead:

1. It publishes the new `Termios` via `termios.swap_commit(new_termios)` — this is visible to subsequent ingest invocations immediately.
2. If the change requires a side-effect on `ldisc_state` (ICANON off → flush cooked_buf; ECHO off → discard echo region), it constructs a closure capturing the required operation and installs it into `ingest_linearizer` via `swap_commit`. If the slot was already Some (a prior closure was pending), the two are composed into one closure that runs both in order.
3. It then wakes the ingest step by firing a synthetic ingest event on the transport wire, ensuring an ingest pass happens soon to drain the linearizer. For a quiescent tty with no byte flow, this is the mechanism that guarantees the termios-change effect eventually applies.

**Bounded-latency property.** The closure applies on the next ingest pass. For active ttys this is microseconds. For quiescent ttys the synthetic-wake ensures an ingest pass within one reactor tick. The closure never waits on external I/O; it's pure-compute mutation on already-held state.

**Why not a mutex + condvar instead.** A mutex would let `tcsetattr` acquire and mutate directly, but at the cost of every ingest byte acquiring the same mutex. Ingest is on the per-byte hot path; tcsetattr is rare. The linearizer inverts the cost: rare path pays for the closure allocation + wake; hot path pays nothing beyond one atomic slot check at commit start.

**Applicability beyond tcsetattr.** Any future operation that needs to mutate `ldisc_state` from outside the ingest step uses the same protocol. Candidates: `TCFLSH` (flush buffers), `TIOCSTI` (inject character — deliberately not implemented per §10, but would use this pattern if it were), certain kinds of pty-close hangup cleanup.

**What this is an instance of.** This is the **single-writer with external-mutator linearizer** pattern noted in §2.6. Whether the pattern recurs in other subsystems (pipe F_SETPIPE_SZ during active I/O; socket setsockopt during active protocol steps) will decide whether the pattern deserves factoring out into a shared primitive. For v1 it is a TTY-local mechanism, documented here.

---

## 4. Job control

<!-- txdoc:TTY-JOB-CONTROL-1 -->

### 4.1 Session and pgrp model

<!-- txdoc:TTY-SESSION-PGRP-MODEL-1 -->

POSIX session/pgrp semantics are preserved: PROCESS owns `Session` and `ProcessGroup` identities; pid namespace sid/pgid numbers are view signifiers. A session may have an optional controlling tty; a controlling tty has a foreground pgrp; signals generated by the tty (from ISIG ldisc processing) deliver to the canonical fg pgrp's members via `deliver_posix_signal`.

The `session_pgrp: AtomicSlot<Option<SessionPgrp>>` field on `TtyIdentity` is the authoritative binding between the terminal and the session/pgrp pair that controls it. `SessionPgrp` stores `Cap<Session>` and `Cap<ProcessGroup>` references; user-facing sid/pgid numbers are resolved before mutation and rendered only at syscall/projection boundaries.

Mutators:

- **TIOCSCTTY**: a session leader with no current controlling tty binds this tty. Publishes `session_pgrp = Some(SessionPgrp { session: caller.session, foreground_pgrp: caller.pgrp })` via `swap_commit`, with the precondition that the slot's prior value was `None`.
- **tcsetpgrp / TIOCSPGRP**: foreground-pgrp change. Resolves the user-supplied pgid through the caller's pid namespace, validates that the target pgrp is in the controlling session, then publishes a new `SessionPgrp` with the updated `foreground_pgrp` via `swap_commit`.
- **tcgetpgrp / TIOCGPGRP**: read-only; reads `session_pgrp.foreground_pgrp` and renders its pgid through the caller's pid namespace.
- **Session leader exits**: process subsystem's `step_exit_commit`, upon detecting `is_session_leader && has_controlling_tty`, calls into tty to clear the binding. `session_pgrp` publishes to `None`; `hangup_port` fires; hangup sequence follows (§4.3).
- **TIOCNOTTY**: caller's session voluntarily disassociates from its controlling tty. Publishes `session_pgrp = None`; caller's `controlling_tty` pointer also clears.

### 4.2 Read / write permission checks

<!-- txdoc:TTY-READ-WRITE-PERMISSION-CHECKS-1 -->

A `read()` from the tty by a process *not* in the fg pgrp delivers `SIGTTIN` (unless blocked/ignored, in which case it blocks until TTIN delivery becomes possible — in v1 we choose to return EIO rather than block indefinitely; a simplification).

A `write()` to the tty by a process not in the fg pgrp delivers `SIGTTOU` if `TOSTOP` is set in termios; otherwise the write succeeds.

These checks are `require_fg_pgrp` in `frame/tty/checks/`, invoked in the observe phase of `step_read` / `step_write`.

### 4.3 Hangup

<!-- txdoc:TTY-HANGUP-1 -->

Triggered by:

- The *master* side of a pty closing (all OpenFiles on the master drop).
- A hardware tty's transport reporting carrier loss (not applicable in v1 — we have no modem-control lines on any target — but the path exists).

Hangup sequence (`tty::step_hangup_commit`):

1. **Snapshot the session/pgrp binding.** Epoch-guarded read of `session_pgrp`. If `None`, skip to step 3 (no session was controlled by this tty; pure payload teardown). If `Some(sp)`, retain `sp.session` and `sp.foreground_pgrp` before payload drop.
2. **Clear the binding.** `session_pgrp.swap_commit(None)` — after this point, new TIOCSPGRP or other binding operations see no controlling session.
3. **Drop payload.** Transition `TtyIdentity.payload` from `Some(PayloadCap)` to `None`. `TtyPayload` reclamation begins (queues freed, ldisc_state dropped, transport's peer-Cap released if pty).
4. **Publish hangup wires.** Fire `hangup_port` — subscribers see `POLLHUP`; pending `step_read` / `step_write` wake on `input_readable` / `output_writable` and observe the transitioned payload via their upgrade attempt returning EIO.
5. **Dispatch cohort signals.** If a session was controlled:
   - `deliver_posix_signal(SignalTarget::ProcessGroup(session_leader_pgrp(session)), SIGHUP, ctx)` — SIGHUP to the session leader's pgrp, per SIGNAL_v1's TTY hangup producer contract.
   - `deliver_posix_signal(SignalTarget::ProcessGroup(foreground_pgrp), SIGCONT, ctx)` — ensure any stopped processes in the foreground pgrp wake to receive the SIGHUP (POSIX §11.1.3 requirement).
6. **Fire `session_ctl_port`** with `LostControllingTty` — for any direct subscribers to session-state-change events on this tty.

Steps 5 and 6 fire within the commit sub-phase of `step_hangup_commit`, per SIG-4. The order of steps 2-5 matters: clearing the binding (2) before dropping payload (3) prevents a concurrent TIOCSPGRP from publishing a new foreground pgrp against an about-to-die payload; dispatching cohort signals (5) after payload drop (3) is acceptable because the cohort signal dispatch retained canonical identities captured at step 1, not live fields of the just-dropped payload.

After hangup, the `TtyIdentity` slot persists as long as any `Cap<TtyIdentity>` is held (open fds, session's controlling-tty pointer). Operations on those fds return EIO — the upgrade from Identity.payload fails because payload is None.

Reclamation happens when:
- All open fds close.
- No session's controlling-tty points at it (i.e., the session-ctl binding withdraws on session exit).

At that point `Cap<TtyIdentity>` refcount hits zero, SENTINEL_DEAD, the TtyIdentity slot reclaims.

---

## 5. Pseudo-terminals

<!-- txdoc:TTY-PSEUDO-TERMINALS-1 -->

### 5.1 Lifecycle

<!-- txdoc:TTY-LIFECYCLE-1 -->

`openpty()` (glibc) → `posix_openpt()` → `open("/dev/ptmx")` → pair creation:

```rust
// frame/tty/execution/step_openpty.rs
pub fn step_openpty(ctx: &Ctx) -> StepOutcome<(Fd, Fd)> {
    // ... (standard observe / upgrade / reserve phases)

    // Allocate identities and payloads — four zone slots.
    let master_id_slot = zone::reserve::<TtyIdentity>()?;
    let slave_id_slot  = zone::reserve::<TtyIdentity>()?;
    let master_pl_slot = zone::reserve::<TtyPayload>()?;
    let slave_pl_slot  = zone::reserve::<TtyPayload>()?;

    // Allocate the next available pty index.
    let pty_index = pty_index_alloc.next_free()?;

    // Construct identities (no payload yet).
    let master_id = TtyIdentity { kind: PtyMaster, index: pty_index, ... };
    let slave_id  = TtyIdentity { kind: PtySlave,  index: pty_index, ... };
    // zone::sign publishes; produces Cap<TtyIdentity> for each.
    let master_id_cap = zone::sign(master_id_slot, master_id);
    let slave_id_cap  = zone::sign(slave_id_slot,  slave_id);

    // Construct payloads, each referencing the peer's identity.
    let master_pl = TtyPayload {
        transport: TtyTransport::Pty { peer: slave_id_cap.clone() },
        ...
    };
    let slave_pl = TtyPayload {
        transport: TtyTransport::Pty { peer: master_id_cap.clone() },
        ...
    };
    let master_pl_cap = zone::sign_payload(master_pl_slot, master_pl);
    let slave_pl_cap  = zone::sign_payload(slave_pl_slot,  slave_pl);

    // Install payloads on identities (commit points).
    index::commit(master_id_cap.payload, master_pl_cap);
    index::commit(slave_id_cap.payload,  slave_pl_cap);

    // Publish slave into devpts so /dev/pts/<pty_index> resolves.
    devpts::register(pty_index, slave_id_cap.clone());

    // Construct two RNodes, two OpenFiles, two fds.
    //   master: StructBacked::Tty(master_id_cap)
    //   slave:  StructBacked::Tty(slave_id_cap)
    // ... (standard fd-allocation path)

    Done((master_fd, slave_fd))
}
```

### 5.2 Master and slave asymmetry

<!-- txdoc:TTY-MASTER-SLAVE-ASYMMETRY-1 -->

The master has **no path** in the filesystem. It's accessible only through the fd returned by `/dev/ptmx`'s open handler (which is a special case — `open("/dev/ptmx")` returns an fd already pointing at a freshly-allocated master).

The slave has a path `/dev/pts/<pty_index>`, projected via devpts (§6).

Closing the master triggers hangup on the slave (per §4.3). Closing the slave does not itself hang up the master, but the master's next read will return EOF / `POLLHUP` once all slaves close.

### 5.3 Pty lifecycle graph

<!-- txdoc:TTY-PTY-LIFECYCLE-GRAPH-1 -->

```
              openpty()
                  │
                  ▼
      ┌──────master ◄──── fd_m ─── user (often shell/sshd)
      │  TtyIdentity
      │  TtyPayload
      │  transport: Pty{peer: slave_id}
      │
      ▼ ring-to-ring
      │
      ▲
      │  TtyIdentity
      │  TtyPayload
      │  transport: Pty{peer: master_id}
      └──────slave ◄───── fd_s ─── user (child login, often bash)
                  ▲
                  │ /dev/pts/<N> (devpts projection)
                  │
              open by path
```

Both sides are full TTYs with ldisc, termios, window size, job-control bindings. ssh's typical use: sshd opens the pty pair, keeps the master, exec's the user shell with the slave as stdin/stdout/stderr, pipes bytes between the master and the network socket.

---

## 6. devpts

<!-- txdoc:TTY-DEVPTS-1 -->

### 6.1 What devpts is

<!-- txdoc:TTY-WHAT-DEVPTS-IS-1 -->

A projection filesystem, in the same shape as procfs: no persistent state, entries derived from a live registry.

- Mount point: `/dev/pts`. Mounted by init after tty::init().
- Static entries: `ptmx` (a pseudo char device that triggers pty-pair allocation on open).
- Dynamic entries: `0`, `1`, `2`, ... — one per allocated pty slave, keyed by `pty_index`.

### 6.2 Lookup

<!-- txdoc:TTY-LOOKUP-1 -->

```rust
// frame/devpts/fs_ops.rs
impl FsOps for DevptsInstance {
    fn lookup(&self, parent: FsObjectId, name: &[u8], guard: &Guard)
        -> StepOutcome<FsObjectId>
    {
        if parent != ROOT { return Err(ENOENT); }
        if name == b"ptmx" { return Done(FsObjectId::PTMX); }
        if let Ok(index) = parse_pty_index(name) {
            if tty::pty_registry::contains(index, guard) {
                return Done(FsObjectId::from_pty_index(index));
            }
        }
        Err(ENOENT)
    }
    // ... readdir enumerates the registry ...
}
```

No name cache — `VopFlags::NO_NAMECACHE`. Lookups are cheap (hash into a small registry) and entries come and go frequently.

### 6.3 RNode materialization

<!-- txdoc:TTY-RNODE-MATERIALIZATION-1 -->

- `ptmx` → `Projected { schema: &PTMX_SCHEMA, key: () }`. The schema's `step_open` is special: it allocates a pty pair and returns an fd on the master. Reads/writes/ioctls go through master-side TTY dispatch.

- `<N>` → `StructBacked { payload: Tty(tty_registry::slave_by_index(N)) }`. The registry returns a fresh `Cap<TtyIdentity>` for the slave's identity.

---

## 7. The hardware-console path

<!-- txdoc:TTY-THE-HARDWARE-CONSOLE-PATH-1 -->

The tier-2 path for `/dev/ttyS0` / `/dev/console`:

```rust
// boards/qemu_riscv64_virt/tty.rs (called from tty::init, phase 6)
pub fn register_tier2_ttys() {
    let ttys0_id = tty::register_hardware(
        "ttyS0",
        DevT::new(MAJ_TTY_S, 0),
        &boards::UART0_BINDING,        // the tier-2 char binding from devices.rs
    );

    // /dev/console is an alias — same TtyIdentity, different devt and name.
    tty::register_alias(
        "console",
        DevT::new(MAJ_TTY, 1),
        ttys0_id.clone(),
    );
}

// frame/tty/execution/register_hardware.rs
pub fn register_hardware(
    name: &str,
    devt: DevT,
    binding: &'static CharDeviceBinding,
) -> Cap<TtyIdentity> {
    // ... reserve slots, construct TtyIdentity + TtyPayload with
    //     transport = Hardware { binding }, publish ...

    // Wire the binding's readable_wire to our input-side ldisc:
    // when the UART IRQ fires readable_wire, a reactor task reads
    // bytes from the binding and feeds them through ldisc_input.
    reactor::spawn_uart_reader(binding, tty_id.clone());

    tty_id
}
```

The hardware tty's lifetime is the kernel's lifetime (DEV-1 corollary: no hardware device goes away). Its `Cap<TtyIdentity>` is held for the lifetime of the kernel by the tty registry; refcount never drops to zero. Session bindings can come and go (via TIOCSCTTY / hangup) without affecting identity retention.

`/dev/console` is a second devfs entry resolving to the *same* `Cap<TtyIdentity>` as `/dev/ttyS0`. Opening either produces a fresh RNode pointing at that Cap; both opens share the same TtyPayload (same termios, same ldisc state).

---

## 8. Signal attachments (contribution to SIGNAL_ATTACHMENTS §3)

<!-- txdoc:TTY-SIGNAL-ATTACHMENTS-CONTRIBUTION-SIGNAL-ATTACHMENTS-3-1 -->

Five attachments for this subsystem. Section in [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) should carry a TTY subsection, supplanting §3.9's partial-placeholder notes where they overlap.

| Entity | Carrier | Wire | Transition | Polarity | Fired from | Subscribers | Projection link |
|---|---|---|---|---|---|---|---|
| `TtyIdentity` | RawQueue | `input_readable` | Input queue non-empty (cooked: after full line) | `set(HasInput)` | `ldisc_input::step_ingest_commit` | poll/select/epoll, `step_read`'s wait | TtyPayload readable-ness |
| `TtyIdentity` | RawQueue | `output_writable` | Output queue has space / transport drained | `set(HasSpace)` | `step_write_commit`, UART IRQ handler | poll/select/epoll, `step_write`'s wait | — |
| `TtyIdentity` | RawPort | `hangup_port` | Hangup complete (payload dropped) | `fire(HungUp)` | `step_hangup_commit` | poll/select/epoll (→ POLLHUP), pidfd-equivalents, controlling session | TtyPayload.payload → false |
| `TtyIdentity` | RawPort | `session_ctl_port` | Session/pgrp change (TIOCSCTTY, setpgid, hangup-reset) | `fire(SessionCtlChange{...})` | `step_set_controlling_commit`, `step_tcsetpgrp_commit`, `step_hangup_commit` | ptrace observers, audit | — (session/pgrp mutation observable through TtyIdentity) |
| `TtyIdentity` (slave only) | RawPort | `hangup_port` (see row 3) | Master peer's last fd closed | `fire(HungUp)` | `step_master_close_last_commit` | same as row 3 | same as row 3 |

Rows 3 and 5 are the same wire fired from different code paths (same polarity, same host); they're listed separately to make the two causes explicit.

BIF-5 compliance: every wire lives on `TtyIdentity`. TtyPayload has no wires of its own. Hangup publication works because Identity outlives the payload it's publishing about.

---

## 9. Module layout

<!-- txdoc:TTY-MODULE-LAYOUT-1 -->

```
frame/tty/
    structure/
        identity.rs           TtyIdentity, TtyKind, SessionPgrp ref type
        payload.rs            TtyPayload, TtyTransport
        termios.rs            Termios struct, bitmask constants
        winsize.rs            Winsize
        ring.rs               TtyRing — the input/output byte ring buffer
        registry.rs           tty_registry — Weak<TtyIdentity> by (kind, index);
                              pty_index_alloc; hardware-tty static slots.
    ldisc/                    Hardcoded N_TTY. Not a trait, not pluggable.
                              See §3 for the LDSC-1 commitment.
        state.rs              LdiscState (cooked_buf, column, flow_stopped, lnext, ...)
        input.rs              process_input_byte — the §3.2 pipeline
        output.rs             process_output — the §3.3 pipeline
        termios_change.rs     on_termios_changed — the §3.4 escape hatch
        effect.rs             LdiscInputEffect enum
    checks/
        require_live_tty.rs   Upgrade-ready witness: TtyIdentity.payload.is_some()
        require_fg_pgrp.rs    Caller in fg pgrp (or override by BG / ignored SIGTTOU)
        require_session_leader.rs
                              For TIOCSCTTY: caller is session leader with no CTT.
    execution/
        step_read.rs          Pull from input_queue (cooked-line ready in ICANON,
                              VMIN/VTIME policy otherwise).
        step_write.rs         ldisc::process_output → output_queue → kick transport.
        step_ioctl.rs         termios get/set (→ ldisc::on_termios_changed on set),
                              TIOCSCTTY, TIOCSPGRP, TIOCGWINSZ, TIOCSWINSZ.
        step_hangup.rs        Drop TtyPayload, fire hangup_port, dispatch SIGHUP
                              through SIGNAL_v1 using canonical ProcessGroup targets.
        step_openpty.rs       Create pty pair. Initialize master termios with most
                              lflag/iflag/oflag bits cleared (raw mode per §3).
        step_master_close.rs  Last-fd-close on master → hangup on slave.
        step_ingest.rs        Reactor-scheduled or peer-invoked. Per byte:
                              ldisc::process_input_byte → dispatch LdiscInputEffect:
                                QueuedForRead | LineCommitted → fire input_readable
                                SignalFgPgrp(sig) → read foreground_pgrp, call deliver_posix_signal
                                FlowControl → adjust transport TX gating
                                Absorbed → nothing further.
        register_hardware.rs  Called by tty::init from board code; constructs a
                              hardware TtyIdentity + TtyPayload with
                              TtyTransport::Hardware { binding } and default
                              cooked-mode termios.
    project.rs                devpts projection entries (ptmx open handler,
                              dynamic pty-slave entries).

frame/devpts/
    fs_ops.rs                 impl FsOps for DevptsInstance (lookup, readdir)
    schema.rs                 PTMX_SCHEMA, pty-slave schema wrappers
```

---

## 10. Open questions

<!-- txdoc:TTY-OPEN-QUESTIONS-1 -->

**10.1 Signal vs EIO on SIGTTIN/TTOU-blocked processes.** Linux blocks; v1 returns EIO after posting TTIN/TTOU. Lossier but simpler. Revisit if porting anything depending on block semantics.

**10.2 Packet mode (TIOCPKT).** Used by `telnet`, `rlogin`, not by ssh. Skipped in v1.

**10.3 Carrier-loss hangup.** Our targets have no modem-control lines. The hangup path from "carrier dropped" is present in code for symmetry but never fires. Wired only for pty master-close in v1.

**10.4 Controlling-tty revocation on exec.** Linux's `O_NOCTTY`-handling on `open` and `exec`-time CTT clearing are spec'd but not detailed here. Covered by process-subsystem spec when it lands; until then, we mirror Linux's behavior.

---

## References

<!-- txdoc:TTY-REFERENCES-1 -->

- [`DEVICE.md`](./DEVICE.md) §5.2 — CharDeviceBinding consumed via `TtyTransport::Hardware`.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) §8.1.1.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — BIF-*, SIG-*.
- [`LIVENESS_v2.1.md`](../00_meta-framework/archived/LIVENESS_v2.1.md) §3.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) §3.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md).
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md).
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — PROCESS-owned `Session` / `ProcessGroup` identities, canonical pgrp/session bindings, and pid namespace rendering rules.
- [`NAMESPACE_VIEW_v1.md`](../00_meta-framework/NAMESPACE_VIEW_v1.md) — pid namespace numbers as view signifiers, not canonical job-control topology.
- Linux kernel: `drivers/tty/n_tty.c` — reference implementation of N_TTY ldisc semantics.
