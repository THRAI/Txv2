.section .text
.globl _start
.type _start, @function
_start:
    # The freestanding C probe uses small-data gp-relative accesses in its
    # signal handler. Linux normally enters CRT code that establishes gp;
    # this hand-written entry stub must provide the same ABI precondition.
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    mv a0, sp
    call vdso_phase5_main
    li a7, 94
    ecall
1:
    j 1b
.size _start, .-_start
