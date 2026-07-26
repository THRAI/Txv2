// ipcbench round 3 — separate "syscall entry cost" from "socket layer cost".
//
// Round 1: pipe == socketpair, throughput linear in chunk size -> fixed per-call cost.
// Round 2: single-process socketpair is ALREADY 3.1 ms/round with no scheduling
//          involved, and getpid() alone costs 221 us. So it is not a wakeup
//          problem; the syscall path itself is expensive.
//
// This round times each primitive with the clock OUTSIDE the loop (round 2's
// per-iteration clock_gettime polluted its own numbers), and adds a non-socket
// fd operation so the syscall-entry cost can be told apart from the socket
// layer:
//
//   getpid           bare syscall entry/exit, no fd, no copy
//   clock_gettime    how much round 2's instrumentation itself cost
//   read /dev/zero   simple fd read, no socket
//   write /dev/null  simple fd write, no socket
//   samepid sp/pipe  socket / pipe write+read, one process
//
// Build: riscv64-linux-musl-gcc -static -O2 -o ipcbench ipcbench.c
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sys/socket.h>

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void report(const char *what, int calls_per_iter, long iters, double secs) {
    printf("IPC:%-22s iters=%-7ld secs=%7.3f us_per_call=%9.2f\n",
           what, iters, secs, secs * 1e6 / (iters * calls_per_iter));
    fflush(stdout);
}

int main(void) {
    const long N = 4000;
    char buf[4096];

    { double t0 = now_s(); for (long i = 0; i < N; i++) (void)getpid();
      report("getpid", 1, N, now_s() - t0); }

    { struct timespec ts; double t0 = now_s();
      for (long i = 0; i < N; i++) clock_gettime(CLOCK_MONOTONIC, &ts);
      report("clock_gettime", 1, N, now_s() - t0); }

    int zfd = open("/dev/zero", O_RDONLY);
    if (zfd >= 0) {
        for (int c = 64; c <= 4096; c *= 64) {
            double t0 = now_s();
            for (long i = 0; i < N; i++) (void)!read(zfd, buf, c);
            char lbl[48]; snprintf(lbl, sizeof lbl, "read /dev/zero c=%d", c);
            report(lbl, 1, N, now_s() - t0);
        }
        close(zfd);
    }

    int nfd = open("/dev/null", O_WRONLY);
    if (nfd >= 0) {
        double t0 = now_s();
        for (long i = 0; i < N; i++) (void)!write(nfd, buf, 64);
        report("write /dev/null c=64", 1, N, now_s() - t0);
        close(nfd);
    }

    // Same process, so no scheduling: pure write+read cost through each layer.
    for (int c = 64; c <= 4096; c *= 64) {
        int sp[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0) {
            double t0 = now_s();
            long ok = 0;
            for (long i = 0; i < N; i++) {
                if (write(sp[0], buf, c) != c) break;
                if (read(sp[1], buf, c) != c) break;
                ok++;
            }
            char lbl[48]; snprintf(lbl, sizeof lbl, "samepid sp c=%d", c);
            report(lbl, 2, ok, now_s() - t0);
            close(sp[0]); close(sp[1]);
        }
        int p[2];
        if (pipe(p) == 0) {
            double t0 = now_s();
            long ok = 0;
            for (long i = 0; i < N; i++) {
                if (write(p[1], buf, c) != c) break;
                if (read(p[0], buf, c) != c) break;
                ok++;
            }
            char lbl[48]; snprintf(lbl, sizeof lbl, "samepid pipe c=%d", c);
            report(lbl, 2, ok, now_s() - t0);
            close(p[0]); close(p[1]);
        }
    }
    printf("IPC:done\n");
    return 0;
}
