#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_tx_trace_on
#define SYS_tx_trace_on 334
#endif

#ifndef SYS_tx_trace_off
#define SYS_tx_trace_off 335
#endif

static long parse_long(const char *text, long fallback)
{
	char *end = 0;
	errno = 0;
	long value = strtol(text, &end, 0);
	if (errno || end == text || *end) return fallback;
	return value;
}

static uint64_t nsec_now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int run_sparse(size_t count, size_t size)
{
	void **ptrs = calloc(count, sizeof(void *));
	if (!ptrs) return 2;
	for (size_t i = 0; i < count; i++) {
		ptrs[i] = malloc(size);
		if (!ptrs[i]) return 3;
		memset(ptrs[i], 0, size);
	}
	for (size_t i = 0; i < count; i++) {
		if (i % 150) free(ptrs[i]);
	}
	free(ptrs);
	return 0;
}

int main(int argc, char **argv)
{
	size_t count = argc > 1 ? (size_t)parse_long(argv[1], 10000) : 10000;
	size_t size = argc > 2 ? (size_t)parse_long(argv[2], 4000) : 4000;
	long trace = argc > 3 ? parse_long(argv[3], 0) : 0;

	printf("malloc-sparse-probe count=%zu size=%zu trace=%ld\n", count, size, trace);
	if (trace) syscall(SYS_tx_trace_on);
	uint64_t start = nsec_now();
	int rc = run_sparse(count, size);
	uint64_t end = nsec_now();
	if (trace) syscall(SYS_tx_trace_off);
	printf("malloc-sparse-probe rc=%d elapsed_ns=%llu\n", rc, (unsigned long long)(end - start));
	return rc;
}
