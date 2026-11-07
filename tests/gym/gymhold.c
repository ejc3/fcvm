/*
 * The fuzz gym's guest workload (#1009): hold one resource across a snapshot and
 * check it every 200 ms, appending one line per check to a log, "<n> ok" or
 * "<n> BAD <what>" for an error or "<n> WRONG <what>" for wrong content, until it is
 * killed. tests/test_fuzz_gym.rs builds it static on the host and ships
 * it through the mapped volume, so the test image needs no tools of its own.
 *
 *   gymhold <log> <kind> <arg>
 *
 * kind   arg        holds                                  each check
 * read   file       fd open O_RDONLY, read sequentially    the block's pattern byte
 * rw     file       fd open O_RDWR                         write block n, read it back
 * append file       fd open O_APPEND                       one line appended
 * mmap   file       MAP_SHARED read-only mapping           the block's pattern byte
 * flock  file       an exclusive flock                     a second open cannot take it
 * notify dir        an inotify watch for IN_CREATE         creating a file is reported
 *                                                          within 5 s
 * listen port       a listening TCP socket                 a connection to it is accepted
 * swperf -          a software perf counter (task clock)   the count grows
 * hwperf -          a hardware perf counter (cycles)       the count grows
 * cwd    name       the working directory (set by the       name, opened relative to it,
 *                   caller)                                reads "inside"
 * dirfd  dir        a descriptor on the directory          its entry "inside", opened
 *                                                          through the descriptor
 * exec   program    nothing                                the program runs and exits 0
 *
 * Two kinds take no log ("-") and do not loop:
 * make   file       write a patterned file of 256 blocks
 * probe  -          exit 0 (what exec runs)
 *
 * Block n of a patterned file holds the byte 'a' + (n % 26) 4096 times, so a read
 * from another file or another offset sees the wrong letter.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <netinet/in.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define BLOCK 4096


static FILE *logf;

static void line(int n, const char *fmt, ...)
{
	va_list ap;
	fprintf(logf, "%d ", n);
	va_start(ap, fmt);
	vfprintf(logf, fmt, ap);
	va_end(ap);
	fputc('\n', logf);
	fflush(logf);
}

/* Each patterned file has its own salt, so a read of another file shows. */
static long salt;
static char want(long block) { return (char)('a' + (block + salt) % 26); }

static sigjmp_buf on_sigbus;
static void caught_sigbus(int sig)
{
	(void)sig;
	siglongjmp(on_sigbus, 1);
}

int main(int argc, char **argv)
{
	if (argc != 4 && argc != 5) {
		fprintf(stderr, "usage: gymhold <log> <kind> <arg> [salt]\n");
		return 2;
	}
	salt = argc == 5 ? atol(argv[4]) : 0;
	const char *kind = argv[2], *arg = argv[3];
	if (!strcmp(kind, "probe"))
		return 0;
	if (!strcmp(kind, "make")) {
		int out = open(arg, O_CREAT | O_TRUNC | O_WRONLY, 0666);
		char block[BLOCK];
		for (long b = 0; out >= 0 && b < 256; b++) {
			memset(block, want(b), BLOCK);
			if (write(out, block, BLOCK) != BLOCK)
				return 1;
		}
		return out >= 0 && close(out) == 0 ? 0 : 1;
	}
	logf = fopen(argv[1], "a");
	if (!logf)
		return 2;
	int fd = -1, fd2 = -1;
	char *map = NULL;
	off_t blocks = 0;
	struct sockaddr_in addr = { .sin_family = AF_INET };

	if (!strcmp(kind, "read") || !strcmp(kind, "rw") || !strcmp(kind, "mmap")) {
		fd = open(arg, !strcmp(kind, "rw") ? O_RDWR : O_RDONLY);
		struct stat st;
		if (fd < 0 || fstat(fd, &st) < 0) {
			line(0, "BAD open %s: %s", arg, strerror(errno));
			return 1;
		}
		blocks = st.st_size / BLOCK;
		if (!strcmp(kind, "mmap")) {
			map = mmap(NULL, st.st_size, PROT_READ, MAP_SHARED, fd, 0);
			if (map == MAP_FAILED) {
				line(0, "BAD mmap: %s", strerror(errno));
				return 1;
			}
		}
	} else if (!strcmp(kind, "append")) {
		fd = open(arg, O_WRONLY | O_APPEND);
	} else if (!strcmp(kind, "flock")) {
		/* NFS emulates flock with fcntl locks, and an exclusive one needs a
		 * descriptor open for writing. */
		fd = open(arg, O_RDWR);
		if (fd < 0)
			fd = open(arg, O_RDONLY);
		if (fd >= 0 && flock(fd, LOCK_EX) < 0) {
			line(0, "BAD flock: %s", strerror(errno));
			return 1;
		}
	} else if (!strcmp(kind, "notify")) {
		fd = inotify_init1(IN_NONBLOCK);
		if (fd >= 0 && inotify_add_watch(fd, arg, IN_CREATE) < 0) {
			line(0, "BAD inotify_add_watch %s: %s", arg, strerror(errno));
			return 1;
		}
	} else if (!strcmp(kind, "listen")) {
		int one = 1;
		fd = socket(AF_INET, SOCK_STREAM, 0);
		addr.sin_port = htons((uint16_t)atoi(arg));
		addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
		setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
		if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(fd, 8) < 0) {
			line(0, "BAD listen: %s", strerror(errno));
			return 1;
		}
	} else if (!strcmp(kind, "cwd") || !strcmp(kind, "exec")) {
		fd = 0;
	} else if (!strcmp(kind, "dirfd")) {
		fd = open(arg, O_RDONLY | O_DIRECTORY);
	} else if (!strcmp(kind, "swperf") || !strcmp(kind, "hwperf")) {
		struct perf_event_attr pe = { .size = sizeof(pe), .exclude_kernel = 1, .exclude_hv = 1 };
		if (!strcmp(kind, "swperf")) {
			pe.type = PERF_TYPE_SOFTWARE;
			pe.config = PERF_COUNT_SW_TASK_CLOCK;
		} else {
			pe.type = PERF_TYPE_HARDWARE;
			pe.config = PERF_COUNT_HW_CPU_CYCLES;
		}
		fd = (int)syscall(SYS_perf_event_open, &pe, 0, -1, -1, 0);
	} else {
		fprintf(stderr, "unknown kind %s\n", kind);
		return 2;
	}
	if (fd < 0) {
		line(0, "BAD %s %s: %s", kind, arg, strerror(errno));
		return 1;
	}

	uint64_t last = 0;
	off_t pos = 0;
	char buf[BLOCK];
	for (int n = 0;; n++) {
		long b = blocks ? n % blocks : 0;
		if (!strcmp(kind, "read")) {
			if (pos >= blocks * BLOCK && lseek(fd, 0, SEEK_SET) == 0)
				pos = 0;
			ssize_t got = read(fd, buf, BLOCK);
			long at = pos / BLOCK;
			pos += got > 0 ? got : 0;
			if (got != BLOCK)
				line(n, "BAD read returned %zd: %s", got, strerror(errno));
			else if (buf[0] != want(at) || buf[BLOCK - 1] != want(at))
				line(n, "WRONG read '%c' at block %ld, want '%c'", buf[0], at, want(at));
			else
				line(n, "ok");
		} else if (!strcmp(kind, "rw")) {
			/* The block holds the file's own pattern until a hold first writes it, and
			 * a hold's letter (upper case) after; another twin or clone may write the
			 * same host file, so any hold's letter is right. Anything else, such as
			 * another file's lower-case pattern, is another file behind this handle,
			 * which a write followed by a read of the same block cannot see. */
			ssize_t had = pread(fd, buf, BLOCK, b * BLOCK);
			if (had != BLOCK) {
				line(n, "BAD pread returned %zd: %s", had, strerror(errno));
				usleep(200000);
				continue;
			}
			if (!(buf[0] == want(b) || (buf[0] >= 'A' && buf[0] <= 'Z')) || buf[BLOCK - 1] != buf[0]) {
				line(n, "WRONG block %ld held '%c' before the write, want '%c' or a hold's letter",
				     b, buf[0], want(b));
				usleep(200000);
				continue;
			}
			char c = (char)('A' + n % 26);
			memset(buf, c, BLOCK);
			if (pwrite(fd, buf, BLOCK, b * BLOCK) != BLOCK) {
				line(n, "BAD pwrite: %s", strerror(errno));
			} else {
				memset(buf, 0, BLOCK);
				ssize_t got = pread(fd, buf, BLOCK, b * BLOCK);
				if (got != BLOCK)
					line(n, "BAD pread returned %zd: %s", got, strerror(errno));
				else if (buf[0] != c || buf[BLOCK - 1] != c)
					line(n, "WRONG pread '%c', want '%c'", buf[0], c);
				else
					line(n, "ok");
			}
		} else if (!strcmp(kind, "append")) {
			char text[32];
			int len = snprintf(text, sizeof(text), "line %d\n", n);
			if (write(fd, text, len) != len)
				line(n, "BAD append: %s", strerror(errno));
			else
				line(n, "ok");
		} else if (!strcmp(kind, "mmap")) {
			/* A page the kernel cannot read in is a SIGBUS: an error, not a crash. */
			volatile char c = 0, e = 0;
			signal(SIGBUS, caught_sigbus);
			if (sigsetjmp(on_sigbus, 1)) {
				line(n, "BAD mmap SIGBUS at block %ld", b);
				usleep(200000);
				continue;
			}
			c = map[b * BLOCK];
			e = map[b * BLOCK + BLOCK - 1];
			if (c != want(b) || e != want(b))
				line(n, "WRONG mmap '%c' at block %ld, want '%c'", c, b, want(b));
			else
				line(n, "ok");
		} else if (!strcmp(kind, "flock")) {
			if (fd2 < 0)
				fd2 = open(arg, O_RDWR);
			if (fd2 < 0)
				fd2 = open(arg, O_RDONLY);
			if (fd2 < 0)
				line(n, "BAD reopen: %s", strerror(errno));
			else if (flock(fd2, LOCK_EX | LOCK_NB) == 0)
				line(n, "BAD a second open took the lock this process holds");
			else if (errno != EWOULDBLOCK)
				line(n, "BAD flock: %s", strerror(errno));
			else
				line(n, "ok");
		} else if (!strcmp(kind, "notify")) {
			char path[4096], name[64];
			snprintf(name, sizeof(name), "created-%d-%d", (int)getpid(), n);
			snprintf(path, sizeof(path), "%s/%s", arg, name);
			int f = open(path, O_CREAT | O_WRONLY, 0644);
			if (f < 0) {
				line(n, "BAD create %s: %s", path, strerror(errno));
			} else {
				close(f);
				unlink(path);
				int seen = 0;
				for (int tries = 0; tries < 500 && !seen; tries++) {
					char ev[4096] __attribute__((aligned(8)));
					ssize_t len = read(fd, ev, sizeof(ev));
					for (char *p = ev; len > 0 && p < ev + len;) {
						struct inotify_event *ie = (struct inotify_event *)p;
						if (ie->len && !strcmp(ie->name, name))
							seen = 1;
						p += sizeof(*ie) + ie->len;
					}
					if (!seen)
						usleep(10000);
				}
				if (seen)
					line(n, "ok");
				else
					line(n, "BAD no IN_CREATE for %s within 5 s", name);
			}
		} else if (!strcmp(kind, "listen")) {
			int c = socket(AF_INET, SOCK_STREAM, 0);
			if (connect(c, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
				line(n, "BAD connect: %s", strerror(errno));
			} else {
				int a = accept(fd, NULL, NULL);
				if (a < 0)
					line(n, "BAD accept: %s", strerror(errno));
				else if (write(c, "x", 1) != 1 || read(a, buf, 1) != 1 || buf[0] != 'x')
					line(n, "BAD no byte through the accepted connection");
				else
					line(n, "ok");
				if (a >= 0)
					close(a);
			}
			close(c);
		} else if (!strcmp(kind, "cwd") || !strcmp(kind, "dirfd")) {
			int f = !strcmp(kind, "cwd") ? open(arg, O_RDONLY) : openat(fd, "inside", O_RDONLY);
			ssize_t got = f >= 0 ? read(f, buf, sizeof(buf)) : -1;
			if (f < 0)
				line(n, "BAD open %s: %s", !strcmp(kind, "cwd") ? arg : "inside", strerror(errno));
			else if (got < 0)
				line(n, "BAD read inside: %s", strerror(errno));
			else if (got != 7 || memcmp(buf, "inside\n", 7))
				line(n, "WRONG read %zd bytes of inside", got);
			else
				line(n, "ok");
			if (f >= 0)
				close(f);
		} else if (!strcmp(kind, "exec")) {
			pid_t child = fork();
			if (child == 0) {
				execl(arg, "gymhold", "-", "probe", "-", (char *)NULL);
				_exit(127);
			}
			int status = 0;
			if (child < 0 || waitpid(child, &status, 0) < 0)
				line(n, "BAD fork or wait: %s", strerror(errno));
			else if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
				line(n, "BAD %s exited with status 0x%x", arg, status);
			else
				line(n, "ok");
		} else {
			uint64_t count = 0;
			for (volatile int spin = 0; spin < 100000; spin++)
				;
			if (read(fd, &count, sizeof(count)) != sizeof(count))
				line(n, "BAD perf read: %s", strerror(errno));
			else if (count <= last)
				line(n, "BAD perf count %llu after %llu", (unsigned long long)count,
				     (unsigned long long)last);
			else
				line(n, "ok");
			last = count;
		}
		usleep(200000);
	}
	return 0;
}
