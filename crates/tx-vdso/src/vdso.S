# RISC-V 64 vDSO — high-resolution clock implementation.
#
# Reads the VVAR page using the image-relative VVAR delta and a seqlock.
#
# Supported clock ids:
#   CLOCK_REALTIME / CLOCK_MONOTONIC (high-res via rdtime + mult/shift)
#   CLOCK_REALTIME_COARSE (5), CLOCK_MONOTONIC_COARSE (6) (coarse seqlock)
# All others return -ENOSYS.
#
# VVAR layout (offsets from base):
#   0x00  seq            (u64)
#   0x08  abi_version    (u32)
#   0x0c  clock_mode     (u32; RiscvTime permits rdtime)
#   0x10  realtime_sec   (u64)
#   0x18  realtime_nsec  (u64, shifted by shift for high-res)
#   0x20  monotonic_sec  (u64)
#   0x28  monotonic_nsec (u64, shifted by shift for high-res)
#   0x30  cycle_last     (u64)
#   0x38  mult           (u64)
#   0x40  shift          (u64)
#   0x48  mask           (u64)

# The build script derives this from VVAR_DELTA, the ELF text vaddr, and the
# actual `auipc` offset in .text. The entry-point then reaches vvar_base from
# its own PC without a runtime virtual address or dynamic relocation.
.ifndef VVAR_PC_DELTA
.set VVAR_PC_DELTA, -4096
.endif

# `build.rs` supplies every VVAR fact from tx-time's exported VvarData ABI.
# Defaults keep direct assembly usable for local inspection only.
.ifndef VVAR_SEQ_OFFSET
.set VVAR_SEQ_OFFSET, 0
.endif
.ifndef VVAR_CLOCK_MODE_OFFSET
.set VVAR_CLOCK_MODE_OFFSET, 12
.endif
.ifndef VVAR_REALTIME_SEC_OFFSET
.set VVAR_REALTIME_SEC_OFFSET, 16
.endif
.ifndef VVAR_REALTIME_NSEC_SHIFTED_OFFSET
.set VVAR_REALTIME_NSEC_SHIFTED_OFFSET, 24
.endif
.ifndef VVAR_MONOTONIC_SEC_OFFSET
.set VVAR_MONOTONIC_SEC_OFFSET, 32
.endif
.ifndef VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET
.set VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET, 40
.endif
.ifndef VVAR_CYCLE_LAST_OFFSET
.set VVAR_CYCLE_LAST_OFFSET, 48
.endif
.ifndef VVAR_MULT_OFFSET
.set VVAR_MULT_OFFSET, 56
.endif
.ifndef VVAR_SHIFT_OFFSET
.set VVAR_SHIFT_OFFSET, 64
.endif
.ifndef VVAR_MASK_OFFSET
.set VVAR_MASK_OFFSET, 72
.endif
.ifndef VDSO_CLOCK_MODE_RISCV_TIME
.set VDSO_CLOCK_MODE_RISCV_TIME, 1
.endif

.section .text, "ax", @progbits
.align 3

# ---------------------------------------------------------------------------
# __vdso_clock_gettime(clk_id in a0, timespec *ts in a1) -> a0
# ---------------------------------------------------------------------------
.globl __vdso_clock_gettime
.type  __vdso_clock_gettime, @function
__vdso_clock_gettime:
.Lclock_gettime_entry:
    addi    sp, sp, -64
    sd      ra, 56(sp)
    sd      s0, 48(sp)
    sd      s1, 40(sp)
    sd      s2, 32(sp)
    sd      s3, 24(sp)
    sd      s4, 16(sp)
    sd      s5, 8(sp)
    sd      s6, 0(sp)

    # Load VVAR base from the clock_gettime PC and image-relative delta.
    auipc   s0, 0
    li      t0, VVAR_PC_DELTA
    add     s0, s0, t0

    # Dispatch on clk_id.
    beqz    a0, .Lgt_highres_realtime    # CLOCK_REALTIME = 0
    addi    t0, zero, 1
    beq     a0, t0, .Lgt_highres_monotonic  # CLOCK_MONOTONIC = 1
    addi    t0, zero, 5            # CLOCK_REALTIME_COARSE
    beq     a0, t0, .Lgt_coarse_realtime
    addi    t0, zero, 6            # CLOCK_MONOTONIC_COARSE
    beq     a0, t0, .Lgt_coarse_monotonic
    li      a0, -38                # -ENOSYS
    j       .Lgt_done

# ---- high-resolution realtime ----
.Lgt_highres_realtime:
    addi    s5, s0, VVAR_REALTIME_SEC_OFFSET
    addi    s6, s0, VVAR_REALTIME_NSEC_SHIFTED_OFFSET
    j       .Lgt_seqlock_highres

# ---- high-resolution monotonic ----
.Lgt_highres_monotonic:
    addi    s5, s0, VVAR_MONOTONIC_SEC_OFFSET
    addi    s6, s0, VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET
    j       .Lgt_seqlock_highres

.Lgt_seqlock_highres:
    # s5 = &sec, s6 = &shifted_nsec. Read the full conversion tuple
    # under one seqlock sample, matching Linux vDSO datapage rules.
1:  ld      t1, VVAR_SEQ_OFFSET(s0) # t1 = seq
    andi    t2, t1, 1
    bnez    t2, 1b
    fence   r, rw
    lwu     t5, VVAR_CLOCK_MODE_OFFSET(s0)
    li      t0, VDSO_CLOCK_MODE_RISCV_TIME
    bne     t5, t0, .Lgt_enosys
    ld      s3, 0(s5)              # s3 = base seconds
    ld      s4, 0(s6)              # s4 = shifted base nanoseconds
    ld      t6, VVAR_CYCLE_LAST_OFFSET(s0)
    ld      t2, VVAR_MASK_OFFSET(s0)
    ld      t3, VVAR_MULT_OFFSET(s0)
    beqz    t3, .Lgt_enosys
    ld      t4, VVAR_SHIFT_OFFSET(s0)
    rdtime  t0                     # t0 = current cycle counter
    fence   r, rw
    ld      t5, VVAR_SEQ_OFFSET(s0) # t5 = seq (re-read)
    bne     t1, t5, 1b

    # Linux-style high-res math:
    # ns = ((delta_cycles * mult) + shifted_base_nsec) >> shift.
    sub     t0, t0, t6
    and     t0, t0, t2
    mulhu   t5, t0, t3             # high 64 bits of 128-bit mul
    mul     t0, t0, t3             # low 64 bits
    add     t0, t0, s4
    sltu    t2, t0, s4
    add     t5, t5, t2
    beqz    t4, .Lgt_shifted_done
    addi    t1, zero, 64
    bltu    t4, t1, .Lgt_shifted_lt64
    sub     t1, t4, t1
    srl     s4, t5, t1
    j       .Lgt_normalize
.Lgt_shifted_lt64:
    sub     t1, t1, t4
    sll     t2, t5, t1
    srl     s4, t0, t4
    or      s4, s4, t2
    j       .Lgt_normalize
.Lgt_shifted_done:
    mv      s4, t0

.Lgt_normalize:
    li      t3, 1000000000
    divu    t2, s4, t3
    remu    s4, s4, t3
    add     s3, s3, t2
    sd      s3, 0(a1)              # ts->tv_sec
    sd      s4, 8(a1)              # ts->tv_nsec
    li      a0, 0
    j       .Lgt_done

# ---- coarse realtime ----
.Lgt_coarse_realtime:
    addi    s1, s0, VVAR_REALTIME_SEC_OFFSET
    addi    s2, s0, VVAR_REALTIME_NSEC_SHIFTED_OFFSET
    j       .Lgt_seqlock_coarse

# ---- coarse monotonic ----
.Lgt_coarse_monotonic:
    addi    s1, s0, VVAR_MONOTONIC_SEC_OFFSET
    addi    s2, s0, VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET

.Lgt_seqlock_coarse:
1:  ld      t0, VVAR_SEQ_OFFSET(s0) # seq
    andi    t1, t0, 1
    bnez    t1, 1b
    fence   r, rw
    lwu     t5, VVAR_CLOCK_MODE_OFFSET(s0)
    li      t1, VDSO_CLOCK_MODE_RISCV_TIME
    bne     t5, t1, .Lgt_enosys
    ld      s3, 0(s1)              # seconds
    ld      s4, 0(s2)              # nanoseconds
    fence   r, rw
    ld      t1, VVAR_SEQ_OFFSET(s0) # seq (re-read)
    bne     t0, t1, 1b
    ld      t2, VVAR_SHIFT_OFFSET(s0) # nsec is stored shifted
    srl     s4, s4, t2
    sd      s3, 0(a1)              # ts->tv_sec
    sd      s4, 8(a1)              # ts->tv_nsec
    li      a0, 0

.Lgt_enosys:
    li      a0, -38                # -ENOSYS

.Lgt_done:
    ld      ra, 56(sp)
    ld      s0, 48(sp)
    ld      s1, 40(sp)
    ld      s2, 32(sp)
    ld      s3, 24(sp)
    ld      s4, 16(sp)
    ld      s5, 8(sp)
    ld      s6, 0(sp)
    addi    sp, sp, 64
    ret
.size __vdso_clock_gettime, .-__vdso_clock_gettime

# ---------------------------------------------------------------------------
# __vdso_gettimeofday(timeval *tv in a0, timezone *tz in a1) -> a0
# ---------------------------------------------------------------------------
.globl __vdso_gettimeofday
.type  __vdso_gettimeofday, @function
__vdso_gettimeofday:
    addi    sp, sp, -48
    sd      ra, 40(sp)
    sd      s0, 32(sp)
    sd      s1, 24(sp)

    mv      s1, a0                 # save timeval*
    li      a0, 0                  # CLOCK_REALTIME
    addi    a1, sp, 0              # scratch timespec
    jal     ra, .Lclock_gettime_entry
    bnez    a0, .Lgtod_done

    ld      t2, 0(sp)              # ts.tv_sec
    ld      t3, 8(sp)              # ts.tv_nsec
    sd      t2, 0(s1)              # tv->tv_sec
    li      t0, 1000
    divu    t3, t3, t0
    sd      t3, 8(s1)              # tv->tv_usec
    li      a0, 0

.Lgtod_done:
    ld      ra, 40(sp)
    ld      s0, 32(sp)
    ld      s1, 24(sp)
    addi    sp, sp, 48
    ret
.size __vdso_gettimeofday, .-__vdso_gettimeofday

# ---------------------------------------------------------------------------
# __vdso_clock_getres(clk_id in a0, timespec *res in a1) -> a0
# ---------------------------------------------------------------------------
.globl __vdso_clock_getres
.type  __vdso_clock_getres, @function
__vdso_clock_getres:
    # CLOCK_REALTIME = 0, CLOCK_MONOTONIC = 1,
    # CLOCK_REALTIME_COARSE = 5, CLOCK_MONOTONIC_COARSE = 6
    addi    t0, zero, 0
    beq     a0, t0, .Lres_hires
    addi    t0, zero, 1
    beq     a0, t0, .Lres_hires
    addi    t0, zero, 5
    beq     a0, t0, .Lres_ok
    addi    t0, zero, 6
    beq     a0, t0, .Lres_ok
    li      a0, -38
    ret

.Lres_hires:
    # High-res: 1 ns resolution.
    li      t0, 0
    li      t1, 1
    sd      t0, 0(a1)
    sd      t1, 8(a1)
    li      a0, 0
    ret

.Lres_ok:
    li      t0, 0
    li      t1, 10000000           # 10 ms coarse resolution
    sd      t0, 0(a1)
    sd      t1, 8(a1)
    li      a0, 0
    ret
.size __vdso_clock_getres, .-__vdso_clock_getres

# ---------------------------------------------------------------------------
# __vdso_rt_sigreturn() -> never returns
# ---------------------------------------------------------------------------
.globl __vdso_rt_sigreturn
.type  __vdso_rt_sigreturn, @function
__vdso_rt_sigreturn:
    li      a7, 139               # __NR_rt_sigreturn
    ecall
.size __vdso_rt_sigreturn, .-__vdso_rt_sigreturn
