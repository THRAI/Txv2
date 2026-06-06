#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    pid_t pid = fork();
    if (pid == 0) {
        puts("child alive");
        exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("parent status=%d\n", status);
    return status;
}
