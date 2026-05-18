# RISC-V 64 vDSO — high-resolution clock implementation.
#
# Reads the VVAR page at 0x3F_FFFE_F000 using a seqlock.
#
# Supported clock ids:
#   CLOCK_REALTIME / CLOCK_MONOTONIC (high-res via rdtime + mult/shift)
#   CLOCK_REALTIME_COARSE (5), CLOCK_MONOTONIC_COARSE (6) (coarse seqlock)
# All others return -ENOSYS.
#
# VVAR layout (offsets from base):
#   0x00  seq            (u64)
#   0x08  realtime_sec   (u64)
#   0x10  realtime_nsec  (u64)
#   0x18  monotonic_sec  (u64)
#   0x20  monotonic_nsec (u64)
#   0x28  cycle_last     (u64)
#   0x30  mult           (u64)
#   0x38  shift          (u64)
#   0x40  mask           (u64)

.section .text, "ax", @progbits
.align 3

# ---------------------------------------------------------------------------
# __vdso_clock_gettime(clk_id in a0, timespec *ts in a1) -> a0
# ---------------------------------------------------------------------------
.globl __vdso_clock_gettime
.type  __vdso_clock_gettime, @function
__vdso_clock_gettime:
    addi    sp, sp, -64
    sd      ra, 56(sp)
    sd      s0, 48(sp)
    sd      s1, 40(sp)
    sd      s2, 32(sp)
    sd      s3, 24(sp)
    sd      s4, 16(sp)
    sd      s5, 8(sp)
    sd      s6, 0(sp)

    # Load VVAR base (0x3F_FFFE_F000) into s0.
    lui     s0, 0xFFFEF
    addi    t0, zero, 64
    slli    t0, t0, 32
    add     s0, s0, t0            # s0 = 0x3F_FFFE_F000

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
    # Read rdtime, compute delta, add to base.
    rdtime  t0                      # t0 = current cycle counter
    ld      t1, 0x28(s0)            # t1 = vvar->cycle_last
    ld      t2, 0x40(s0)            # t2 = vvar->mask
    sub     t0, t0, t1
    and     t0, t0, t2              # t0 = delta cycles

    ld      t3, 0x30(s0)            # t3 = mult
    ld      t4, 0x38(s0)            # t4 = shift
    mulhu   t5, t0, t3              # high 64 bits of 128-bit mul
    mul     t0, t0, t3              # low 64 bits
    # 128-bit (t5:t0) >> t4
    beqz    t4, .Lgt_ns_noshift
    addi    t1, zero, 64
    bltu    t4, t1, .Lgt_shift_lt64
    # shift >= 64: result = t5 >> (t4 - 64)
    sub     t1, t4, t1
    srl     t0, t5, t1
    j       .Lgt_ns_done
.Lgt_shift_lt64:
    sub     t1, t1, t4
    sll     t2, t5, t1
    srl     t0, t0, t4
    or      t0, t0, t2
    j       .Lgt_ns_done
.Lgt_ns_noshift:
    mv      t0, t5
.Lgt_ns_done:

    # Add to realtime base with seqlock.
    j       .Lgt_add_to_base

# ---- high-resolution monotonic ----
.Lgt_highres_monotonic:
    rdtime  t0
    ld      t1, 0x28(s0)
    ld      t2, 0x40(s0)
    sub     t0, t0, t1
    and     t0, t0, t2

    ld      t3, 0x30(s0)
    ld      t4, 0x38(s0)
    mulhu   t5, t0, t3
    mul     t0, t0, t3
    beqz    t4, .Lgt_mn_noshift
    addi    t1, zero, 64
    bltu    t4, t1, .Lgt_mn_lt64
    sub     t1, t4, t1
    srl     t0, t5, t1
    j       .Lgt_mn_done
.Lgt_mn_lt64:
    sub     t1, t1, t4
    sll     t2, t5, t1
    srl     t0, t0, t4
    or      t0, t0, t2
    j       .Lgt_mn_done
.Lgt_mn_noshift:
    mv      t0, t5
.Lgt_mn_done:

    # Add to monotonic base with seqlock.
    # s1/s2 point to sec/nsec base.
    addi    s5, s0, 0x18           # monotonic_sec
    addi    s6, s0, 0x20           # monotonic_nsec
    j       .Lgt_seqlock_add

.Lgt_add_to_base:
    addi    s5, s0, 0x08           # realtime_sec
    addi    s6, s0, 0x10           # realtime_nsec

.Lgt_seqlock_add:
    # t0 = ns delta, s5 = &sec, s6 = &nsec
1:  ld      t1, 0(s0)              # t1 = seq
    andi    t2, t1, 1
    bnez    t2, 1b
    fence   r, rw
    ld      s3, 0(s5)              # s3 = base seconds
    ld      s4, 0(s6)              # s4 = base nanoseconds
    fence   r, rw
    ld      t2, 0(s0)              # t2 = seq (re-read)
    bne     t1, t2, 1b

    # Add ns delta to base nsec, normalize.
    add     s4, s4, t0
    li      t3, 1000000000
    bltu    s4, t3, .Lgt_no_overflow
    sub     s4, s4, t3
    addi    s3, s3, 1
.Lgt_no_overflow:
    sd      s3, 0(a1)              # ts->tv_sec
    sd      s4, 8(a1)              # ts->tv_nsec
    li      a0, 0
    j       .Lgt_done

# ---- coarse realtime ----
.Lgt_coarse_realtime:
    addi    s1, s0, 8              # &realtime_sec
    addi    s2, s0, 16             # &realtime_nsec
    j       .Lgt_seqlock_coarse

# ---- coarse monotonic ----
.Lgt_coarse_monotonic:
    addi    s1, s0, 24             # &monotonic_sec
    addi    s2, s0, 32             # &monotonic_nsec

.Lgt_seqlock_coarse:
1:  ld      t0, 0(s0)              # seq
    andi    t1, t0, 1
    bnez    t1, 1b
    fence   r, rw
    ld      s3, 0(s1)              # seconds
    ld      s4, 0(s2)              # nanoseconds
    fence   r, rw
    ld      t1, 0(s0)              # seq (re-read)
    bne     t0, t1, 1b
    sd      s3, 0(a1)              # ts->tv_sec
    sd      s4, 8(a1)              # ts->tv_nsec
    li      a0, 0

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
    addi    sp, sp, -32
    sd      ra, 24(sp)
    sd      s0, 16(sp)
    sd      s1, 8(sp)

    # Load VVAR base.
    lui     s0, 0xFFFEF
    addi    t0, zero, 64
    slli    t0, t0, 32
    add     s0, s0, t0

    # Read realtime sec (offset 8) and nsec (offset 16) with seqlock.
    addi    s1, s0, 8

1:  ld      t0, 0(s0)
    andi    t1, t0, 1
    bnez    t1, 1b
    fence   r, rw
    ld      t2, 0(s1)              # t2 = seconds
    ld      t3, 16(s0)             # t3 = nsec
    fence   r, rw
    ld      t1, 0(s0)
    bne     t0, t1, 1b

    sd      t2, 0(a0)              # tv->tv_sec
    li      t0, 1000
    divu    t3, t3, t0
    sd      t3, 8(a0)              # tv->tv_usec
    li      a0, 0

    ld      ra, 24(sp)
    ld      s0, 16(sp)
    ld      s1, 8(sp)
    addi    sp, sp, 32
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
