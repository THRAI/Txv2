// Runner that exec's libctest with "argv" test
#include <unistd.h>
int main(void) {
    execl("/pthread_test", "entry-static.exe", "argv", NULL);
    return 1;
}
