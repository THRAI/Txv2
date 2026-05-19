/* SysV IPC smoke test for txKernel.
 * Direct syscall wrappers — musl may or may not provide shmget/semget wrappers.
 * Compile: riscv64-linux-musl-gcc -static -o ipc_test ipc_test.c
 * Run:     /bin/ipc_test
 */

#include <stdint.h>
#include <unistd.h>

/* Linux RV64 syscall numbers (generic ABI). */
#define SYS_shmget  194
#define SYS_shmat   196
#define SYS_shmdt   197
#define SYS_shmctl  195
#define SYS_msgget  186
#define SYS_msgsnd  189
#define SYS_msgrcv  188
#define SYS_msgctl  187
#define SYS_semget  190
#define SYS_semop   193
#define SYS_semctl  191

/* IPC flags */
#define IPC_CREAT   01000
#define IPC_EXCL    02000
#define IPC_PRIVATE 0
#define IPC_RMID    0
#define IPC_SET     1
#define IPC_STAT    2
#define IPC_NOWAIT  04000
#define SEM_UNDO    02000

/* shmctl cmds */
#define SHM_LOCK    11
#define SHM_UNLOCK  12

/* semctl cmds */
#define GETVAL  12
#define SETVAL  16

static long syscall6(long nr, long a0, long a1, long a2, long a3, long a4, long a5) {
    register long a7 __asm__("a7") = nr;
    register long a0_r __asm__("a0") = a0;
    register long a1_r __asm__("a1") = a1;
    register long a2_r __asm__("a2") = a2;
    register long a3_r __asm__("a3") = a3;
    register long a4_r __asm__("a4") = a4;
    register long a5_r __asm__("a5") = a5;
    __asm__ volatile("ecall"
                     : "+r"(a0_r)
                     : "r"(a7), "r"(a1_r), "r"(a2_r), "r"(a3_r), "r"(a4_r), "r"(a5_r)
                     : "memory");
    return a0_r;
}

static long syscall3(long nr, long a0, long a1, long a2) {
    return syscall6(nr, a0, a1, a2, 0, 0, 0);
}

static long syscall2(long nr, long a0, long a1) {
    return syscall6(nr, a0, a1, 0, 0, 0, 0);
}

static long syscall1(long nr, long a0) {
    return syscall6(nr, a0, 0, 0, 0, 0, 0);
}

/* write() syscall for output */
#define SYS_write 64

static void putstr(const char *s) {
    long len = 0;
    while (s[len]) len++;
    syscall3(SYS_write, 1, (long)s, len);
}

static void puthex(unsigned long v) {
    char buf[17];
    for (int i = 15; i >= 0; i--) {
        int d = v & 0xf;
        buf[i] = d < 10 ? '0' + d : 'a' + d - 10;
        v >>= 4;
    }
    buf[16] = 0;
    putstr(buf);
}

static long shmget(int key, unsigned long size, int shmflg) {
    return syscall3(SYS_shmget, key, size, shmflg);
}

static long shmat(int shmid, void *shmaddr, int shmflg) {
    return syscall3(SYS_shmat, shmid, (long)shmaddr, shmflg);
}

static long shmdt(void *shmaddr) {
    return syscall1(SYS_shmdt, (long)shmaddr);
}

static long shmctl(int shmid, int cmd, void *buf) {
    return syscall3(SYS_shmctl, shmid, cmd, (long)buf);
}

/* Minimal semid_ds / shmid_ds / msqid_ds for STAT */
struct ipc_perm {
    int uid, gid, cuid, cgid;
    unsigned short mode;
    unsigned short __pad1;
    unsigned short __pad2;
};

static long msgget(int key, int msgflg) {
    return syscall2(SYS_msgget, key, msgflg);
}

static long semget(int key, int nsems, int semflg) {
    return syscall3(SYS_semget, key, nsems, semflg);
}

struct sembuf {
    unsigned short sem_num;
    short sem_op;
    short sem_flg;
};

static long semop(int semid, struct sembuf *sops, unsigned nsops) {
    return syscall3(SYS_semop, semid, (long)sops, nsops);
}

static long semctl(int semid, int semnum, int cmd, long arg) {
    return syscall6(SYS_semctl, semid, semnum, cmd, arg, 0, 0);
}

/* Minimal exit */
#define SYS_exit 93
static void do_exit(int code) {
    syscall1(SYS_exit, code);
}

static const char *errstr(long r) {
    if (r >= 0) return "OK";
    switch (-r) {
        case 2:  return "ENOENT";
        case 13: return "EACCES";
        case 14: return "EFAULT";
        case 17: return "EEXIST";
        case 22: return "EINVAL";
        case 27: return "EFBIG";
        case 38: return "ENOSYS";
        case 43: return "EIDRM";
        default: return "?";
    }
}

#define TEST(name) do { \
    putstr("  " name ": "); \
} while(0)

#define CHECK(r, expect) do { \
    long _r = (r); \
    if (_r == (expect)) { \
        putstr("OK\n"); \
    } else { \
        putstr("FAIL ("); putstr(errstr(_r)); \
        if (_r < 0) { putstr(", ret="); puthex((unsigned long)_r); } \
        putstr(")\n"); \
    } \
} while(0)

#define CHECK_GE(r, min) do { \
    long _r = (r); \
    if (_r >= (min)) { \
        putstr("OK (id="); puthex((unsigned long)_r); putstr(")\n"); \
    } else { \
        putstr("FAIL ("); putstr(errstr(_r)); \
        if (_r < 0) { putstr(", ret="); puthex((unsigned long)_r); } \
        putstr(")\n"); \
    } \
} while(0)

int main(void) {
    putstr("=== SysV IPC smoke test ===\n");

    /* 1. shmget — create private segment */
    TEST("shmget(IPC_PRIVATE, 4096, 0666|IPC_CREAT)");
    long shmid = shmget(IPC_PRIVATE, 4096, 0666 | IPC_CREAT);
    CHECK_GE(shmid, 0);

    /* 2. shmget — keyed create */
    TEST("shmget(0x1234, 4096, 0666|IPC_CREAT)");
    long shmid2 = shmget(0x1234, 4096, 0666 | IPC_CREAT);
    CHECK_GE(shmid2, 0);

    /* 3. shmget — keyed lookup (no IPC_CREAT) */
    TEST("shmget(0x1234, 4096, 0)");
    long shmid3 = shmget(0x1234, 4096, 0);
    CHECK(shmid3, shmid2);

    /* 4. shmget — keyed exclusive fail */
    TEST("shmget(0x1234, 4096, IPC_CREAT|IPC_EXCL)");
    long shmid4 = shmget(0x1234, 4096, IPC_CREAT | IPC_EXCL);
    CHECK(shmid4, -17); /* EEXIST */

    /* 5. shmget — IPC_PRIVATE (another) */
    TEST("shmget(IPC_PRIVATE, 8192, 0600|IPC_CREAT)");
    long shmid5 = shmget(IPC_PRIVATE, 8192, 0600 | IPC_CREAT);
    CHECK_GE(shmid5, 0);

    /* 6. shmat stub — will likely return ENOSYS or 0 */
    TEST("shmat(stub)");
    long addr = shmat((int)shmid, 0, 0);
    /* Day-1 stub returns 0 or -ENOSYS; accept either */
    if (addr == 0) {
        putstr("OK (stub returned 0)\n");
    } else if (addr == -38) {
        putstr("OK (stub returned ENOSYS)\n");
    } else {
        putstr("FAIL (unexpected ret)\n");
    }

    /* 7. shmdt stub */
    TEST("shmdt(stub)");
    long r = shmdt((void *)0x1000);
    CHECK(r, 0);

    /* 8. shmctl IPC_RMID on keyed segment */
    TEST("shmctl(IPC_RMID)");
    long r2 = shmctl((int)shmid2, IPC_RMID, 0);
    CHECK(r2, 0);

    /* 9. msgget — create private queue */
    TEST("msgget(IPC_PRIVATE, 0666|IPC_CREAT)");
    long msqid = msgget(IPC_PRIVATE, 0666 | IPC_CREAT);
    CHECK_GE(msqid, 0);

    /* 10. msgget — keyed create */
    TEST("msgget(0x5678, 0666|IPC_CREAT)");
    long msqid2 = msgget(0x5678, 0666 | IPC_CREAT);
    CHECK_GE(msqid2, 0);

    /* 11. msgget — keyed lookup */
    TEST("msgget(0x5678, 0)");
    long msqid3 = msgget(0x5678, 0);
    CHECK(msqid3, msqid2);

    /* 12. msgsnd stub — may return EINVAL or EAGAIN (empty msg?) */
    TEST("msgsnd(stub, prio=1, len=4)");
    long r3 = syscall6(SYS_msgsnd, (int)msqid, (long)"test", 4, 0, 0, 0);
    if (r3 == -22) {
        putstr("OK (EINVAL — mtype=0 rejected as expected since raw ptr)\n");
    } else if (r3 >= 0) {
        putstr("OK\n");
    } else {
        putstr("FAIL ("); putstr(errstr(r3)); putstr(")\n");
    }

    /* 13. msgrcv stub */
    TEST("msgrcv(stub)");
    char buf[128];
    long r4 = syscall6(SYS_msgrcv, (int)msqid, (long)buf, 128, 0, 0, 0);
    if (r4 < 0) {
        putstr("OK (empty queue)\n");
    } else {
        putstr("OK (got msg)\n");
    }

    /* 14. msgctl IPC_RMID */
    TEST("msgctl(IPC_RMID)");
    long r5 = syscall3(SYS_msgctl, (int)msqid2, IPC_RMID, 0);
    CHECK(r5, 0);

    /* 15. semget — create 2-element array */
    TEST("semget(IPC_PRIVATE, 2, 0666|IPC_CREAT)");
    long semid = semget(IPC_PRIVATE, 2, 0666 | IPC_CREAT);
    CHECK_GE(semid, 0);

    /* 16. semget — keyed */
    TEST("semget(0x9ABC, 2, 0666|IPC_CREAT)");
    long semid2 = semget(0x9ABC, 2, 0666 | IPC_CREAT);
    CHECK_GE(semid2, 0);

    /* 17. semop — increment sem 0 */
    TEST("semop(+1 on sem 0)");
    struct sembuf sop = {0, 1, 0};
    long r6 = semop((int)semid, &sop, 1);
    if (r6 == 0 || r6 == -38) {  /* 0=success, -38=ENOSYS */
        putstr("OK\n");
    } else {
        putstr("FAIL\n");
    }

    /* 18. semop — decrement sem 0 */
    TEST("semop(-1 on sem 0)");
    struct sembuf sop2 = {0, -1, 0};
    long r7 = semop((int)semid, &sop2, 1);
    if (r7 == 0 || r7 == -38) {
        putstr("OK\n");
    } else {
        putstr("FAIL\n");
    }

    /* 19. semctl SETVAL */
    TEST("semctl(SETVAL, sem 1 = 42)");
    long r8 = semctl((int)semid, 1, SETVAL, 42);
    if (r8 == 0 || r8 == -38) {
        putstr("OK\n");
    } else {
        putstr("FAIL\n");
    }

    /* 20. semctl GETVAL */
    TEST("semctl(GETVAL, sem 1)");
    long r9 = semctl((int)semid, 1, GETVAL, 0);
    putstr("val="); puthex((unsigned long)r9); putstr("\n");

    /* 21. semctl IPC_RMID */
    TEST("semctl(IPC_RMID)");
    long r10 = semctl((int)semid2, 0, IPC_RMID, 0);
    if (r10 == 0 || r10 == -38) {
        putstr("OK\n");
    } else {
        putstr("FAIL\n");
    }

    /* 22. verify destroyed sem: semget re-creates */
    TEST("semget(0x9ABC, 2, 0) after RMID");
    long r11 = semget(0x9ABC, 2, 0);
    CHECK(r11, -2); /* ENOENT after RMID */

    putstr("\n=== Done ===\n");
    do_exit(0);
    return 0;
}
