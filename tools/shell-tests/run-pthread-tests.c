// Run a libctest test directly
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        printf("Usage: %s <testname>\n", argv[0]);
        return 1;
    }
    // exec the entry-static.exe with the test name
    execl("/entry-static.exe", "entry-static.exe", argv[1], NULL);
    perror("execl");
    return 1;
}
