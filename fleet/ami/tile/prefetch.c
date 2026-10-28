/* Read the pieces of the root volume's files that a tile's first boot reads, as early as
 * possible and many at a time (fleet/ami/tile/fcvm-fleet-prefetch.service).
 *
 *   prefetch LIST [THREADS [PIECE_BYTES [ROOT]]]
 *
 * LIST holds `path<TAB>offset<TAB>length` lines (fleet/ami/tile/resident.py writes it from a
 * real first boot). The root volume of an instance made from the AMI is fetched from S3 one
 * 512 KiB block at a time, on the first read of each, so a boot that reads a file one request
 * after another waits for each block in turn. This reads the same pieces THREADS at a time
 * in the list's order, so the order puts what the boot needs first up front.
 *
 * A C program, linked statically, because it is the first thing a boot runs from that volume:
 * an interpreter or a dynamic loader would read tens of files of its own, one block after
 * another, before the first piece of the list. A listed file the image lacks is skipped.
 * ROOT (default empty) is prepended to every path, to read a volume mounted elsewhere.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct task {
	char *path;
	off_t off;
	size_t len;
};

static struct task *tasks;
static size_t ntasks;
static atomic_size_t next_task;
static atomic_ullong bytes_read;
static atomic_ulong files_failed;
/* The most one task reads: a longer list line is several tasks, so its threads share it. */
static size_t chunk = 1 << 20;
/* A thread's read buffer: the pieces are read in steps of it, so 512 threads hold 64 MiB. */
#define STEP (128 << 10)
static const char *root = "";

static void *worker(void *arg)
{
	char *buf = malloc(STEP);
	(void)arg;
	if (!buf)
		return NULL;
	for (;;) {
		size_t i = atomic_fetch_add(&next_task, 1);
		if (i >= ntasks)
			break;
		struct task *t = &tasks[i];
		char full[4352];
		snprintf(full, sizeof full, "%s%s", root, t->path);
		int fd = open(full, O_RDONLY | O_NOATIME | O_NONBLOCK);
		if (fd < 0 && errno == EPERM)
			fd = open(full, O_RDONLY | O_NONBLOCK);
		if (fd < 0) {
			atomic_fetch_add(&files_failed, 1);
			continue;
		}
		size_t done = 0;
		while (done < t->len) {
			ssize_t n = pread(fd, buf, t->len - done < STEP ? t->len - done : STEP, t->off + done);
			if (n <= 0)
				break;
			done += n;
		}
		atomic_fetch_add(&bytes_read, done);
		close(fd);
	}
	free(buf);
	return NULL;
}

/* Add the pieces of one list line, none longer than chunk. */
static int add(const char *path, off_t off, size_t len)
{
	static size_t cap;
	for (size_t at = 0; at < len; at += chunk) {
		if (ntasks == cap) {
			cap = cap ? cap * 2 : 4096;
			struct task *grown = realloc(tasks, cap * sizeof *tasks);
			if (!grown)
				return -1;
			tasks = grown;
		}
		tasks[ntasks].path = strdup(path);
		tasks[ntasks].off = off + at;
		tasks[ntasks].len = len - at < chunk ? len - at : chunk;
		if (!tasks[ntasks].path)
			return -1;
		ntasks++;
	}
	return 0;
}

static long long now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1000LL + ts.tv_nsec / 1000000;
}

int main(int argc, char **argv)
{
	if (argc < 2) {
		fprintf(stderr, "usage: prefetch LIST [THREADS [PIECE_BYTES [ROOT]]]\n");
		return 2;
	}
	long threads = argc > 2 ? atol(argv[2]) : 128;
	if (argc > 3 && atol(argv[3]) > 0)
		chunk = atol(argv[3]);
	if (argc > 4)
		root = argv[4];
	if (threads < 1 || threads > 4096) {
		fprintf(stderr, "fcvm-fleet-prefetch: THREADS must be 1 to 4096\n");
		return 2;
	}
	FILE *f = fopen(argv[1], "r");
	if (!f) {
		fprintf(stderr, "fcvm-fleet-prefetch: no %s\n", argv[1]);
		return 0;
	}
	long long t0 = now_ms();
	char *line = NULL;
	size_t cap = 0;
	unsigned long long listed = 0;
	while (getline(&line, &cap, f) > 0) {
		char *tab1 = strchr(line, '\t');
		if (!tab1)
			continue;
		*tab1 = 0;
		char *end;
		long long off = strtoll(tab1 + 1, &end, 10);
		if (*end != '\t')
			continue;
		long long len = strtoll(end + 1, NULL, 10);
		if (off < 0 || len <= 0)
			continue;
		if (add(line, off, len) < 0) {
			fprintf(stderr, "fcvm-fleet-prefetch: out of memory\n");
			return 1;
		}
		listed += len;
	}
	fclose(f);
	pthread_t *ids = calloc(threads, sizeof *ids);
	long started = 0;
	for (long i = 0; i < threads; i++)
		if (pthread_create(&ids[i], NULL, worker, NULL) == 0)
			started++;
	if (started == 0) {
		fprintf(stderr, "fcvm-fleet-prefetch: cannot start a thread\n");
		return 1;
	}
	for (long i = 0; i < threads; i++)
		if (ids[i])
			pthread_join(ids[i], NULL);
	unsigned long long got = atomic_load(&bytes_read);
	printf("fcvm-fleet-prefetch: read %llu of %llu listed bytes (%llu MiB) in %zu pieces with %ld threads in %lld ms (%lu files not opened)\n",
	       got, listed, got >> 20, ntasks, started, now_ms() - t0, (unsigned long)atomic_load(&files_failed));
	return 0;
}
