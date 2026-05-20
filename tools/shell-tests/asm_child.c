// Child entry: just ecall(exit_group, 0) — no stack, no function call
// This is raw RV64 machine code embedded as a function pointer
__asm__ (
".section .text\n"
".globl child_entry_asm\n"
"child_entry_asm:\n"
"    li a7, 94\n"
"    li a0, 0\n"
"    ecall\n"
);
void child_entry_asm(void);

// Test main — clone and let child call exit_group
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/mman.h>

static long raw_clone(unsigned long flags, void *stack, int *ptid, void *tls, int *ctid) {
    register long a0 __asm__("a0") = flags;
    register long a1 __asm__("a1") = (long)stack;
    register long a2 __asm__("a2") = (long)ptid;
    register long a3 __asm__("a3") = (long)tls;
    register long a4 __asm__("a4") = (long)ctid;
    register long a7 __asm__("a7") = 220;
    __asm__ volatile ("ecall" : "+r"(a0) : "r"(a1),"r"(a2),"r"(a3),"r"(a4),"r"(a7) : "memory");
    return a0;
}
#define CLONE_VM          0x00000100
#define CLONE_THREAD      0x00010000
#define CLONE_SIGHAND     0x00000800
#define CLONE_SETTLS      0x00080000
#define CLONE_CHILD_CLEARTID 0x00200000
#define CLONE_PARENT_SETTID  0x00100000
#define CLONE_DETACHED    0x00400000
static int parent_tid_storage;
static int child_tid_storage;

int main(void) {
    printf("[main] pid=%d tid=%d\n", getpid(), (int)syscall(SYS_gettid));
    size_t stack_size = 4096;
    void *stack = mmap(NULL, stack_size, PROT_READ|PROT_WRITE,
                       MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) { printf("mmap fail\n"); return 1; }
    void *child_stack = (char*)stack + stack_size;
    child_stack = (void*)((unsigned long)child_stack & ~15UL);
    child_stack = (char*)child_stack - 16;
    // Store the raw asm entry as the child "function"
    *(void**)((char*)child_stack + 0) = child_entry_asm;
    *(void**)((char*)child_stack + 8) = (void*)0;
    unsigned long flags = CLONE_VM | CLONE_THREAD | CLONE_SIGHAND 
                        | CLONE_SETTLS | CLONE_CHILD_CLEARTID 
                        | CLONE_PARENT_SETTID | CLONE_DETACHED;
    void *tls = (char*)stack + 2048;
    long ret = raw_clone(flags, child_stack, &parent_tid_storage, tls, &child_tid_storage);
    if (ret < 0) { printf("[main] clone FAIL: %ld\n", ret); return 1; }
    printf("[main] clone OK: child_tid=%ld\n", ret);
    for (;;) { syscall(SYS_gettid); }
}
