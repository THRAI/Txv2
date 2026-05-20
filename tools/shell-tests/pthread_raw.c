// pthread test with raw error code display
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/syscall.h>

void *worker(void *arg) {
    printf("[pthread] worker %d running\n", (int)(long)arg);
    return NULL;
}

int main(void) {
    printf("[pthread] main: pid=%d tid=%d\n", getpid(), (int)syscall(SYS_gettid));
    printf("[pthread] main: creating worker 1...\n");
    
    pthread_t t1;
    int ret = pthread_create(&t1, NULL, worker, (void*)1);
    if (ret != 0) {
        printf("[pthread] FAIL: pthread_create t1: ret=%d errno=%d (%s)\n", ret, errno, strerror(ret));
    } else {
        printf("[pthread] OK: t1 created, joining...\n");
        pthread_join(t1, NULL);
        printf("[pthread] t1 joined\n");
    }
    
    printf("[pthread] main: exit\n");
    return 0;
}
