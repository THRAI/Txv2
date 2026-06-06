#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
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

static void *worker(void *arg)
{
	(void)arg;
	return 0;
}

static long now_ns(void)
{
	struct timespec ts;
	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return -1;
	return ts.tv_sec * 1000000000L + ts.tv_nsec;
}

static long parse_long(const char *text, long fallback)
{
	if (!text || !*text) return fallback;
	char *end = 0;
	errno = 0;
	long value = strtol(text, &end, 0);
	if (errno || end == text || *end) return fallback;
	return value;
}

int main(int argc, char **argv)
{
	long outer = argc > 1 ? parse_long(argv[1], 1) : 1;
	long inner = argc > 2 ? parse_long(argv[2], 50) : 50;
	long stack_size = argc > 3 ? parse_long(argv[3], 16384) : 16384;
	long trace = argc > 4 ? parse_long(argv[4], 0) : 0;
	if (outer <= 0 || inner <= 0 || stack_size < 4096) {
		fprintf(stderr, "usage: %s [outer] [inner] [stack_size] [trace]\n", argv[0]);
		return 2;
	}

	pthread_t *threads = calloc((size_t)inner, sizeof(*threads));
	pthread_attr_t *attrs = calloc((size_t)inner, sizeof(*attrs));
	void **stacks = calloc((size_t)inner, sizeof(*stacks));
	if (!threads || !attrs || !stacks) {
		perror("calloc");
		return 1;
	}

	for (long i = 0; i < inner; i++) {
		int ret = posix_memalign(&stacks[i], 16, (size_t)stack_size);
		if (ret) {
			fprintf(stderr, "posix_memalign(%ld): %s\n", i, strerror(ret));
			return 1;
		}
		ret = pthread_attr_init(&attrs[i]);
		if (ret) {
			fprintf(stderr, "pthread_attr_init(%ld): %s\n", i, strerror(ret));
			return 1;
		}
		ret = pthread_attr_setstack(&attrs[i], stacks[i], (size_t)stack_size);
		if (ret) {
			fprintf(stderr, "pthread_attr_setstack(%ld): %s\n", i, strerror(ret));
			return 1;
		}
	}

	if (trace) syscall(SYS_tx_trace_on);
	long start = now_ns();
	for (long batch = 0; batch < outer; batch++) {
		for (long i = 0; i < inner; i++) {
			int ret = pthread_create(&threads[i], &attrs[i], worker, (void *)(uintptr_t)i);
			if (ret) {
				fprintf(stderr, "pthread_create batch=%ld i=%ld: %s\n", batch, i, strerror(ret));
				return 1;
			}
		}
		for (long i = 0; i < inner; i++) {
			int ret = pthread_join(threads[i], 0);
			if (ret) {
				fprintf(stderr, "pthread_join batch=%ld i=%ld: %s\n", batch, i, strerror(ret));
				return 1;
			}
		}
	}
	long end = now_ns();
	if (trace) syscall(SYS_tx_trace_off);

	printf("static_stack outer=%ld inner=%ld stack=%ld time_ns=%ld\n",
	       outer, inner, stack_size, end - start);
	return 0;
}
