// Direct clone test — bypass musl pthread, test kernel clone directly
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/syscall.h>

// RV64 clone syscall signature: (flags, stack, ptid, tls, ctid) -> child_tid
static long raw_clone(unsigned long flags, void *stack, int *ptid, void *tls, int *ctid) {
    register long a0 __asm__("a0") = flags;
    register long a1 __asm__("a1") = (long)stack;
    register long a2 __asm__("a2") = (long)ptid;
    register long a3 __asm__("a3") = (long)tls;
    register long a4 __asm__("a4") = (long)ctid;
    register long a7 __asm__("a7") = 220; // __NR_clone
    
    __asm__ volatile (
        "ecall"
        : "+r"(a0)
        : "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a7)
        : "memory"
    );
    return a0;
}

#define CLONE_VM          0x00000100
#define CLONE_THREAD      0x00010000
#define CLONE_SIGHAND     0x00000800
#define CLONE_SETTLS      0x00080000
#define CLONE_CHILD_CLEARTID 0x00200000
#define CLONE_PARENT_SETTID  0x00100000
#define CLONE_DETACHED    0x00400000

static int child_tid_storage;
static int parent_tid_storage;

int main(void) {
    printf("[main] pid=%d tid=%d\n", getpid(), (int)syscall(SYS_gettid));
    
    unsigned long flags = CLONE_VM | CLONE_THREAD | CLONE_SIGHAND 
                        | CLONE_SETTLS | CLONE_CHILD_CLEARTID 
                        | CLONE_PARENT_SETTID | CLONE_DETACHED;
    
    printf("[main] raw_clone flags=0x%lx\n", flags);
    
    long ret = raw_clone(flags, NULL, &parent_tid_storage, NULL, &child_tid_storage);
    
    if (ret < 0) {
        printf("[main] raw_clone FAILED: ret=%ld errno=%d (%s)\n", ret, errno, strerror(errno));
        printf("[main] child_tid_storage=%d parent_tid_storage=%d\n", child_tid_storage, parent_tid_storage);
    } else {
        printf("[main] raw_clone OK: child_tid=%ld\n", ret);
    }
    
    return 0;
}
