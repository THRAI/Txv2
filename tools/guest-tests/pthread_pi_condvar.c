#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static pthread_mutex_t mutex;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;

static void say(const char *msg)
{
    size_t len = strlen(msg);
    while (len > 0) {
        ssize_t n = write(1, msg, len);
        if (n <= 0) {
            return;
        }
        msg += n;
        len -= (size_t)n;
    }
}

static void die_pthread(const char *what, int err)
{
    char buf[256];
    int n = snprintf(buf, sizeof(buf), "FAIL %s: %s (%d)\n", what, strerror(err), err);
    if (n > 0) {
        if ((size_t)n >= sizeof(buf)) {
            n = (int)sizeof(buf) - 1;
        }
        (void)write(1, buf, (size_t)n);
    }
}

static void *worker(void *arg)
{
    (void)arg;

    int err = pthread_mutex_lock(&mutex);
    if (err) {
        die_pthread("worker pthread_mutex_lock", err);
        return (void *)1;
    }

    ready = 1;
    err = pthread_cond_signal(&cond);
    if (err) {
        die_pthread("worker pthread_cond_signal", err);
        return (void *)1;
    }

    err = pthread_mutex_unlock(&mutex);
    if (err) {
        die_pthread("worker pthread_mutex_unlock", err);
        return (void *)1;
    }

    return 0;
}

int main(void)
{
    say("#### TX GUEST TEST START pthread-pi-condvar ####\n");

    pthread_mutexattr_t attr;
    int err = pthread_mutexattr_init(&attr);
    if (err) {
        die_pthread("pthread_mutexattr_init", err);
        return 1;
    }

    err = pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT);
    if (err) {
        die_pthread("pthread_mutexattr_setprotocol(PRIO_INHERIT)", err);
        return 1;
    }

    err = pthread_mutex_init(&mutex, &attr);
    if (err) {
        die_pthread("pthread_mutex_init", err);
        return 1;
    }

    err = pthread_mutexattr_destroy(&attr);
    if (err) {
        die_pthread("pthread_mutexattr_destroy", err);
        return 1;
    }

    err = pthread_mutex_lock(&mutex);
    if (err) {
        die_pthread("main pthread_mutex_lock", err);
        return 1;
    }

    pthread_t thread;
    err = pthread_create(&thread, 0, worker, 0);
    if (err) {
        die_pthread("pthread_create", err);
        return 1;
    }

    while (!ready) {
        err = pthread_cond_wait(&cond, &mutex);
        if (err) {
            die_pthread("pthread_cond_wait", err);
            return 1;
        }
    }

    err = pthread_mutex_unlock(&mutex);
    if (err) {
        die_pthread("main pthread_mutex_unlock", err);
        return 1;
    }

    void *ret = 0;
    err = pthread_join(thread, &ret);
    if (err) {
        die_pthread("pthread_join", err);
        return 1;
    }
    if (ret != 0) {
        char buf[64];
        int n = snprintf(buf, sizeof(buf), "FAIL worker returned %ld\n", (long)ret);
        if (n > 0) {
            if ((size_t)n >= sizeof(buf)) {
                n = (int)sizeof(buf) - 1;
            }
            (void)write(1, buf, (size_t)n);
        }
        return 1;
    }

    err = pthread_mutex_destroy(&mutex);
    if (err) {
        die_pthread("pthread_mutex_destroy", err);
        return 1;
    }

    err = pthread_cond_destroy(&cond);
    if (err) {
        die_pthread("pthread_cond_destroy", err);
        return 1;
    }

    say("PASS pthread-pi-condvar\n");
    say("#### TX GUEST TEST END pthread-pi-condvar ####\n");
    return 0;
}
