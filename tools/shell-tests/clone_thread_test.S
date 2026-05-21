# RV64 assembly: test clone(CLONE_THREAD|CLONE_VM|CLONE_SIGHAND|CLONE_SETTLS) directly
# Build: riscv64-linux-musl-gcc -nostdlib -static -o clone_thread_test clone_thread_test.S
# (or use zig cc with appropriate flags)

.section .text
.globl _start
_start:
    # write(1, msg_main, 14)
    li a7, 64
    li a0, 1
    lla a1, msg_main
    li a2, 14
    ecall

    # sched_setscheduler(0, 0, NULL) — musl calls this in pthread_create
    li a7, 119
    li a0, 0
    li a1, 0
    li a2, 0
    ecall

    # set_tid_address(clear_child_tid)
    li a7, 96
    lla a0, clear_child_tid
    ecall

    # set_robust_list(&robust_head, 24)
    li a7, 99
    lla a0, robust_head
    li a1, 24
    ecall

    # clone(CLONE_VM|CLONE_THREAD|CLONE_SIGHAND|CLONE_SETTLS|CLONE_CHILD_CLEARTID|CLONE_PARENT_SETTID|SIGCHLD, child_stack, &parent_tid, &tls_val, &clear_child_tid)
    # flags = 0x17d00f00 | 0x11 = 0x17d00f11
    li a7, 220
    lui a0, 0x17d01
    addiw a0, a0, -239     # a0 = 0x17d00f11
    # CLONE_VM|CLONE_THREAD|CLONE_SIGHAND|CLONE_SETTLS|CLONE_CHILD_CLEARTID|CLONE_PARENT_SETTID|SIGCHLD
    lla a1, child_stack_top
    lla a2, parent_tid
    lla a3, tls_val
    lla a4, clear_child_tid
    ecall

    # parent: a0 = child tid (or negative errno)
    mv s0, a0
    bltz a0, clone_failed

    # write(1, msg_parent, 20)
    li a7, 64
    li a0, 1
    lla a1, msg_parent
    li a2, 20
    ecall

    # wait for child thread to finish
    # futex(FUTEX_WAIT, &child_done, 0, NULL)
    li a7, 98
    lla a0, child_done
    li a1, 0          # FUTEX_WAIT
    li a2, 0
    li a3, 0
    ecall

    # write(1, msg_done, 14)
    li a7, 64
    li a0, 1
    lla a1, msg_done
    li a2, 14
    ecall

    # exit(0)
    li a7, 93
    li a0, 0
    ecall

clone_failed:
    # write(1, msg_fail, 18)
    li a7, 64
    li a0, 1
    lla a1, msg_fail
    li a2, 18
    ecall
    # exit(1)
    li a7, 93
    li a0, 1
    ecall

# Child thread entry point
.globl child_fn
child_fn:
    # write(1, msg_child, 19)
    li a7, 64
    li a0, 1
    lla a1, msg_child
    li a2, 19
    ecall

    # set child_done = 1
    li t0, 1
    lla t1, child_done
    sw t0, 0(t1)

    # futex(FUTEX_WAKE, &child_done, 1)
    li a7, 98
    lla a0, child_done
    li a1, 1          # FUTEX_WAKE
    li a2, 1
    li a3, 0
    ecall

    # exit(0)
    li a7, 93
    li a0, 0
    ecall

.section .data
msg_main: .ascii "[main] starting\n"
msg_parent: .ascii "[parent] clone ok\n"
msg_child: .ascii "[child] thread ok\n"
msg_done: .ascii "[main] all ok\n"
msg_fail: .ascii "[main] clone FAIL\n"

.section .bss
.align 4
clear_child_tid: .zero 4
parent_tid: .zero 4
child_done: .zero 4
tls_val: .zero 8
robust_head: .zero 24

.align 16
child_stack: .zero 4096
child_stack_top:
