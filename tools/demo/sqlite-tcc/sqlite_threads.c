#include <pthread.h>
#include <stdio.h>
#include <stdint.h>

static void *worker(void *arg) {
    uintptr_t id = (uintptr_t)arg;
    printf("worker %lu says hello\n", (unsigned long)id);
    return (void *)(id + 10);
}

int main(void) {
    pthread_t th;
    void *ret = 0;

    puts("main: creating pthread");

    if (pthread_create(&th, 0, worker, (void *)3) != 0) {
        puts("pthread_create failed");
        return 1;
    }

    if (pthread_join(th, &ret) != 0) {
        puts("pthread_join failed");
        return 2;
    }

    printf("main: pthread returned %lu\n", (unsigned long)(uintptr_t)ret);
    return 0;
}
