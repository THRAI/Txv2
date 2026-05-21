// Simple pthread smoke test for txKernel
// Compile: riscv64-linux-musl-gcc -static -pthread -o pthread_test pthread_test.c
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>

static int shared_counter = 0;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;

static void* worker(void* arg) {
    int id = *(int*)arg;
    printf("[pthread] worker %d: tid=%lu start\n", id, (unsigned long)pthread_self());

    for (int i = 0; i < 100; i++) {
        pthread_mutex_lock(&mutex);
        shared_counter++;
        pthread_mutex_unlock(&mutex);
    }

    printf("[pthread] worker %d: tid=%lu done, counter=%d\n", id, (unsigned long)pthread_self(), shared_counter);
    return (void*)(long)(id * 1000);
}

int main(void) {
    printf("[pthread] main: pid=%d tid=%lu\n", getpid(), (unsigned long)pthread_self());

    pthread_t t1, t2;
    int id1 = 1, id2 = 2;

    printf("[pthread] main: creating worker 1...\n");
    if (pthread_create(&t1, NULL, worker, &id1) != 0) {
        printf("[pthread] FAIL: pthread_create t1: %s\n", strerror(errno));
        return 1;
    }

    printf("[pthread] main: creating worker 2...\n");
    if (pthread_create(&t2, NULL, worker, &id2) != 0) {
        printf("[pthread] FAIL: pthread_create t2: %s\n", strerror(errno));
        return 1;
    }

    printf("[pthread] main: waiting for workers...\n");

    void* ret1 = NULL;
    void* ret2 = NULL;
    pthread_join(t1, &ret1);
    pthread_join(t2, &ret2);

    printf("[pthread] main: worker 1 returned %ld, worker 2 returned %ld\n",
           (long)ret1, (long)ret2);
    printf("[pthread] main: final counter=%d (expected 200)\n", shared_counter);

    if (shared_counter == 200) {
        printf("[pthread] PASS\n");
        return 0;
    } else {
        printf("[pthread] FAIL: counter=%d != 200\n", shared_counter);
        return 1;
    }
}
