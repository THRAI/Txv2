// Alpine gcompat lacks these glibc compatibility symbols needed by the copied
// RV64 rustc/cargo toolchain. Keep the aliases inside the WORKLOAD-only shim.
#include <fcntl.h>
#include <stdarg.h>

extern int res_init(void);

int __res_init(void) { return res_init(); }

static int fcntl_needs_argument(int command) {
    switch (command) {
    case F_DUPFD:
    case F_DUPFD_CLOEXEC:
    case F_SETFD:
    case F_SETFL:
    case F_SETOWN:
    case F_SETSIG:
    case F_SETLEASE:
    case F_NOTIFY:
    case F_SETPIPE_SZ:
    case F_ADD_SEALS:
    case F_GETLK:
    case F_SETLK:
    case F_SETLKW:
    case F_OFD_GETLK:
    case F_OFD_SETLK:
    case F_OFD_SETLKW:
        return 1;
    default:
        return 0;
    }
}

int fcntl64(int fd, int command, ...) {
    va_list args;
    if (!fcntl_needs_argument(command)) return fcntl(fd, command);
    va_start(args, command);
    unsigned long argument = va_arg(args, unsigned long);
    va_end(args);
    return fcntl(fd, command, argument);
}
