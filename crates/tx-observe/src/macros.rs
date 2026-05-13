//! `traced_syscall!` macro — L0 boundary observation wrapper (OBS-3a).
//!
//! # Usage
//!
//! ```ignore
//! pub async fn sys_write<'a, P: TxPlatform>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
//!     traced_syscall!(P, NR_WRITE as u32, args, SyscallResult::to_exit_fields, {
//!         // existing sys_write body
//!     })
//! }
//! ```
//!
//! The macro expands to:
//! 1. Try to get the current hart's emitter via `tx_observe::current()` (D16:
//!    platform context is implicit via the cpu-id function pointer installed
//!    at boot; the `$Plat` parameter is accepted but no longer needed for
//!    `current()` — retained for call-site compatibility).
//! 2. If present, emit `SpanBegin` + `PayloadSyscallEnter` at L0 (Boundary).
//! 3. Run the body expression (`.await`-able).
//! 4. If emitter was present, encode exit payload and emit `SpanEnd` + `PayloadSyscallExit`.
//! 5. Return the body's result.
//!
//! # Anti-pattern compliance
//!
//! This macro is placed at the U/K boundary (syscall entry / exit), which is
//! the only valid L0 emit site per OBS-V1-CONVERGENCE. It must NOT be called
//! from inside a `StepOp::step` body (OBS-A-1).
//!
//! Spec refs:
//!   txdoc:OBS-V1-HOOKS-1  — L0 emit site
//!   txdoc:OBS-V1-ANTI-1   — OBS-A-1 anti-pattern

/// Wrap a syscall body with L0 `SpanBegin` / `SpanEnd` observation records.
///
/// # Parameters
///
/// - `$Plat` — a type implementing `PercpuIf` (accepted for call-site
///   compatibility; `current()` no longer needs a type parameter — D16).
/// - `$sysno` — `u32` Linux syscall number.
/// - `$args` — `[u64; 6]` register-shaped syscall arguments (first 3 logged
///   as `argc=3` in `PayloadSyscallEnter`; full ArgCont records are OBS-3b).
/// - `$to_exit` — a function/closure `|result: &R| -> (i64, i32, u8)` that
///   maps the syscall result to `(ret, errno, result_kind)` for the exit payload.
/// - `$body` — expression producing the syscall result of type `R`.
///
/// # Emitted records
///
/// `SpanBegin`:
/// - level = L0 (Boundary)
/// - payload_tag = SyscallEnter
/// - PayloadSyscallEnter { sysno, abi: 0, argc: 3 }
///
/// `SpanEnd`:
/// - payload_tag = SyscallExit
/// - PayloadSyscallExit { ret, errno, result_kind }
///
/// When no emitter is registered, the body runs with zero overhead beyond the
/// `current()` check (a single array-index load).
#[macro_export]
macro_rules! traced_syscall {
    ($Plat:ty, $sysno:expr, $args:expr, $to_exit:expr, $body:expr) => {{
        use $crate::SpanId;

        // ── L0 SpanBegin ────────────────────────────────────────────────────
        let span: SpanId = if let Some(em) = $crate::current() {
            use $crate::encode::{encode_syscall_enter, syscall_enter_tag};
            use $crate::{EventNameId, PayloadSyscallEnter, TxTraceLevel};

            let enter = PayloadSyscallEnter {
                sysno: $sysno as u32,
                abi: 0,
                argc: 3,
            };
            let (payload_bytes, _) = encode_syscall_enter(&enter);
            em.span_begin(
                TxTraceLevel::Boundary,
                EventNameId::from_raw($sysno as u32),
                SpanId::NONE,
                syscall_enter_tag(),
                &payload_bytes,
            )
        } else {
            SpanId::NONE
        };

        // ── Syscall body ────────────────────────────────────────────────────
        let result = $body;

        // ── L0 SpanEnd ──────────────────────────────────────────────────────
        if span != SpanId::NONE {
            if let Some(em) = $crate::current() {
                use $crate::encode::{encode_syscall_exit, syscall_exit_tag};
                use $crate::PayloadSyscallExit;

                let (ret_val, errno_val, kind_val): (i64, i32, u8) = ($to_exit)(&result);
                let exit = PayloadSyscallExit {
                    ret: ret_val,
                    errno: errno_val,
                    result_kind: kind_val,
                    _pad: [0u8; 3],
                };
                let (payload_bytes, _) = encode_syscall_exit(&exit);
                em.span_end(span, syscall_exit_tag(), &payload_bytes);
            }
        }

        result
    }};
}
