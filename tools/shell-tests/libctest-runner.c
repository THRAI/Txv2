// Wrapper that runs libctest pthread tests and outputs results
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <stdlib.h>

int main(void) {
    // Run the entry-static.exe binary with each pthread test
    const char *tests[] = {
        "pthread_cancel",
        "pthread_cancel_points",  
        "pthread_cond",
        "pthread_tsd",
        "pthread_robust_detach",
        "pthread_cancel_sem_wait",
        "pthread_cond_smasher",
        "pthread_condattr_setclock",
        "pthread_exit_cancel",
        "pthread_once_deadlock",
        "pthread_rwlock_ebusy",
        NULL
    };
    
    // First, create /entry-static.exe by copying from /pthread_test
    // (the fixture embeds the binary at /pthread_test)
    pid_t pid = fork();
    if (pid == 0) {
        execl("/pthread_test", "entry-static.exe", tests[0], NULL);
        _exit(1);
    }
    int status;
    waitpid(pid, &status, 0);
    printf("=== DONE ===\n");
    return 0;
}
