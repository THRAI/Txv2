// Minimal ELF for txKernel — no libc, raw syscalls only.
// Linker script: none needed (position-independent entry)

__attribute__((naked))
void _start(void) {
    // write(1, msg, 12) — __NR_write = 64
    register long a7 __asm__("a7") = 64;   // syscall number
    register long a0 __asm__("a0") = 1;    // fd = stdout
    register const char *a1 __asm__("a1") = "hello world\n";
    register long a2 __asm__("a2") = 12;   // count
    __asm__ volatile("ecall");
    
    // exit(0) — __NR_exit = 93
    register long a7_e __asm__("a7") = 93;
    register long a0_e __asm__("a0") = 0;
    __asm__ volatile("ecall");
    
    __builtin_unreachable();
}
