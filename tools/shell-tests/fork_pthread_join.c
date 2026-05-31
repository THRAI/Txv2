#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static void *worker(void *arg) {
    puts("worker alive");
    return NULL;
}

int main(void) {
    pid_t pid = fork();
    if (pid == 0) {
        puts("child before pthread");
        fflush(stdout);
        pthread_t t;
        int rc = pthread_create(&t, NULL, worker, NULL);
        printf("pthread_create=%d\n", rc);
        if (rc == 0) {
            pthread_join(t, NULL);
            puts("child joined");
        }
        exit(rc);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("parent status=%d\n", status);
    return status;
}
