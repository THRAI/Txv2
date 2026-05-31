#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    pid_t pid = fork();
    if (pid == 0) {
        struct timespec ts;
        puts("child before clock");
        clock_gettime(CLOCK_REALTIME, &ts);
        printf("child clock=%ld.%09ld\n", (long)ts.tv_sec, ts.tv_nsec);
        exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("parent status=%d\n", status);
    return status;
}
