#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;
static int done;

static void *worker(void *arg)
{
    (void)arg;
    puts("probe:worker:start");
    if (pthread_mutex_lock(&lock) != 0) {
        puts("probe:worker:lock-fail");
        return (void *)1;
    }
    ready = 1;
    puts("probe:worker:waiting");
    while (!done) {
        int rc = pthread_cond_wait(&cond, &lock);
        if (rc != 0) {
            printf("probe:worker:condwait-rc=%d\n", rc);
            pthread_mutex_unlock(&lock);
            return (void *)2;
        }
    }
    pthread_mutex_unlock(&lock);
    puts("probe:worker:done");
    return 0;
}

int main(void)
{
    pthread_t thread;
    puts("probe:main:start");
    if (pthread_create(&thread, 0, worker, 0) != 0) {
        puts("probe:main:create-fail");
        return 1;
    }

    for (int i = 0; i < 1000000; i++) {
        pthread_mutex_lock(&lock);
        int seen = ready;
        pthread_mutex_unlock(&lock);
        if (seen) {
            break;
        }
        if ((i % 10000) == 0) {
            usleep(1000);
        }
    }

    pthread_mutex_lock(&lock);
    if (!ready) {
        puts("probe:main:ready-timeout");
        pthread_mutex_unlock(&lock);
        return 2;
    }
    done = 1;
    puts("probe:main:signal");
    pthread_cond_signal(&cond);
    pthread_mutex_unlock(&lock);

    void *result = 0;
    int join_rc = pthread_join(thread, &result);
    printf("probe:main:join-rc=%d result=%ld\n", join_rc, (long)result);
    if (join_rc != 0 || result != 0) {
        return 3;
    }
    puts("probe:result:pass");
    return 0;
}
