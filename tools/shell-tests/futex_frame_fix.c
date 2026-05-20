// Futex-based clone test with musl frame size adjustment
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <unistd.h>

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

static long futex(int *addr, int op, int val, void *timeout) {
    register long a0 __asm__("a0") = (long)addr;
    register long a1 __asm__("a1") = op;
    register long a2 __asm__("a2") = val;
    register long a3 __asm__("a3") = (long)timeout;
    register long a7 __asm__("a7") = 98;
    __asm__ volatile ("ecall" : "+r"(a0) : "r"(a1),"r"(a2),"r"(a3),"r"(a7) : "memory");
    return a0;
}

#define CLONE_VM          0x00000100
#define CLONE_THREAD      0x00010000
#define CLONE_SIGHAND     0x00000800
#define CLONE_SETTLS      0x00080000
#define CLONE_CHILD_CLEARTID 0x00200000
#define CLONE_PARENT_SETTID  0x00100000
#define CLONE_DETACHED    0x00400000

static int child_done;
static int parent_tid_storage;
static int child_tid_storage;

static int child_fn(void *arg) {
    const char msg[] = "OK\n";
    register long a0 __asm__("a0") = 1;
    register long a1 __asm__("a1") = (long)msg;
    register long a2 __asm__("a2") = 3;
    register long a7 __asm__("a7") = 64;
    __asm__ volatile ("ecall" : "+r"(a0) : "r"(a1),"r"(a2),"r"(a7) : "memory");
    child_done = 1;
    futex(&child_done, 1, 1, NULL);  // FUTEX_WAKE
    register long a7e __asm__("a7") = 93;
    register long a0e __asm__("a0") = 0;
    __asm__ volatile ("ecall" : : "r"(a0e),"r"(a7e) : "memory");
    __builtin_unreachable();
}

int main(void) {
    printf("[main] pid=%d tid=%d\n", getpid(), (int)syscall(SYS_gettid));
    size_t stack_size = 65536;
    void *stack = mmap(NULL, stack_size, PROT_READ|PROT_WRITE,
                       MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) { printf("mmap fail\n"); return 1; }
    
    // Top of child's stack. The musl __clone wrapper has a 112-byte frame
    // (addi sp,sp,-112). After ecall, the child uses sp-relative loads
    // (ld ra,104(sp)). The kernel sets sp = stack_arg. So stack_arg
    // must be (top_of_stack - frame_size) so that 104(sp) stays within
    // the allocation.
    void *child_stack = (char*)stack + stack_size - 112;
    child_stack = (void*)((unsigned long)child_stack & ~15UL);
    // Store fn/arg — the wrapper expects these at sp+0 and sp+8 AFTER
    // frame restoration, but the wrapper uses s0-relative addressing
    // which we handle via s0=sp fix in the kernel.
    *(void**)((char*)child_stack + 104) = child_fn;   // where ra was saved
    *(void**)((char*)child_stack + 96) = (void*)0;    // where s0 was saved
    
    unsigned long flags = CLONE_VM | CLONE_THREAD | CLONE_SIGHAND 
                        | CLONE_SETTLS | CLONE_CHILD_CLEARTID 
                        | CLONE_PARENT_SETTID | CLONE_DETACHED;
    void *tls = (char*)stack + 4096;
    
    long ret = raw_clone(flags, child_stack, &parent_tid_storage, tls, &child_tid_storage);
    if (ret < 0) { printf("[main] clone FAIL: %ld\n", ret); return 1; }
    printf("[main] clone OK: child_tid=%ld\n", ret);
    
    while (child_done == 0) { futex(&child_done, 0, 0, NULL); }
    printf("[main] child done\n");
    return 0;
}
