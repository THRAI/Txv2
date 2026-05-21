// Minimal clone child test — child does one write syscall with embedded string
#include <stdio.h>
#include <unistd.h>

// Assembly child entry: write(1, "OK\n", 3); exit(0)
__asm__ (
".section .text\n"
".globl child_entry\n"
"child_entry:\n"
"    li a7, 64\n"
"    li a0, 1\n"
"    auipc a1, 0\n"
"    addi a1, a1, 12\n"
"    li a2, 3\n"
"    ecall\n"
"    li a7, 93\n"
"    li a0, 0\n"
"    ecall\n"
".ascii \"OK\\n\"\n"
);

void child_entry(void);

int main(void) {
    printf("[main] pid=%d\n", getpid());
    
    unsigned long flags = 0x790900;
    
    register long a0 __asm__("a0") = flags;
    register long a1 __asm__("a1") = 0;
    register long a2 __asm__("a2") = 0;
    register long a3 __asm__("a3") = 0;
    register long a4 __asm__("a4") = 0;
    register long a7 __asm__("a7") = 220;
    
    __asm__ volatile ("ecall" : "+r"(a0) : "r"(a1),"r"(a2),"r"(a3),"r"(a4),"r"(a7) : "memory");
    
    if (a0 < 0)
        printf("[main] clone FAIL: %ld\n", a0);
    else
        printf("[main] clone OK: child_tid=%ld\n", a0);
    
    return 0;
}
