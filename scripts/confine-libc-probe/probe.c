/* For each libc function, the syscalls it makes: a marker write(-1, name) precedes each call,
 * and strace -f prints the calls in order. Built once against glibc and once against musl. */
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define M(name) (void)!write(-1, name, sizeof(name) - 1)
static void *thr(void *a) { return a; }
static void on_sig(int s) { (void)s; }

int main(void) {
    char buf[4096]; struct stat st; struct timespec ts = {0, 1000};
    /* run.sh starts it in a directory of its own. */
    int efd = epoll_create1(0); struct epoll_event ev;
    M("epoll_create"); { int x = epoll_create(1); close(x); }
    M("epoll_create1"); { int x = epoll_create1(EPOLL_CLOEXEC); close(x); }
    M("epoll_wait"); epoll_wait(efd, &ev, 1, 0);
    M("epoll_pwait"); epoll_pwait(efd, &ev, 1, 0, NULL);
    M("open"); { int x = open("f", O_RDWR | O_CREAT, 0600); close(x); }
    M("openat"); { int x = openat(AT_FDCWD, "f", O_RDONLY); close(x); }
    M("creat"); { int x = creat("g", 0600); close(x); }
    M("fopen"); { FILE *f = fopen("f", "r"); fclose(f); }
    int fd = open("f", O_RDWR);
    M("stat"); stat("f", &st);
    M("lstat"); lstat("f", &st);
    M("fstat"); fstat(fd, &st);
    M("fstatat"); fstatat(AT_FDCWD, "f", &st, 0);
    M("access"); access("f", R_OK);
    M("faccessat"); faccessat(AT_FDCWD, "f", R_OK, 0);
    M("statfs"); { struct statfs s; statfs(".", &s); }
    M("fstatfs"); { struct statfs s; fstatfs(fd, &s); }
    M("statvfs"); { struct statvfs s; statvfs(".", &s); }
    M("fstatvfs"); { struct statvfs s; fstatvfs(fd, &s); }
    M("poll"); { struct pollfd p = {fd, POLLIN, 0}; poll(&p, 1, 0); }
    M("ppoll"); { struct pollfd p = {fd, POLLIN, 0}; struct timespec z = {0, 0}; ppoll(&p, 1, &z, NULL); }
    M("select"); { fd_set r; FD_ZERO(&r); FD_SET(fd, &r); struct timeval z = {0, 0}; select(fd + 1, &r, 0, 0, &z); }
    M("pselect"); { fd_set r; FD_ZERO(&r); FD_SET(fd, &r); struct timespec z = {0, 0}; pselect(fd + 1, &r, 0, 0, &z, NULL); }
    M("dup"); close(dup(fd));
    M("dup2"); dup2(fd, 100); close(100);
    M("dup3"); dup3(fd, 101, O_CLOEXEC); close(101);
    M("nanosleep"); nanosleep(&ts, NULL);
    M("clock_nanosleep"); clock_nanosleep(CLOCK_MONOTONIC, 0, &ts, NULL);
    M("clock_nanosleep_realtime"); clock_nanosleep(CLOCK_REALTIME, 0, &ts, NULL);
    M("usleep"); usleep(1);
    M("clock_gettime"); clock_gettime(CLOCK_MONOTONIC, &ts);
    M("clock_gettime_boottime"); clock_gettime(CLOCK_BOOTTIME, &ts);
    M("gettimeofday"); { struct timeval tv; gettimeofday(&tv, NULL); }
    M("time"); (void)time(NULL);
    M("pipe"); { int p[2]; (void)!pipe(p); close(p[0]); close(p[1]); }
    M("pipe2"); { int p[2]; (void)!pipe2(p, O_CLOEXEC); close(p[0]); close(p[1]); }
    M("link"); link("f", "h");
    M("unlink"); unlink("h");
    M("unlinkat"); unlinkat(AT_FDCWD, "g", 0);
    M("symlink"); (void)!symlink("f", "s");
    M("readlink"); (void)!readlink("s", buf, sizeof buf);
    M("readlinkat"); (void)!readlinkat(AT_FDCWD, "s", buf, sizeof buf);
    M("rename"); rename("s", "t");
    M("renameat"); renameat(AT_FDCWD, "t", AT_FDCWD, "s"); unlink("s");
    M("mkdir"); mkdir("dd", 0700);
    M("mkdirat"); mkdirat(AT_FDCWD, "de", 0700);
    M("rmdir"); rmdir("dd"); rmdir("de");
    M("chmod"); chmod("f", 0600);
    M("fchmod"); fchmod(fd, 0600);
    M("chown"); (void)!chown("f", -1, -1);
    M("fchown"); (void)!fchown(fd, -1, -1);
    M("truncate"); (void)!truncate("f", 8192);
    M("ftruncate"); (void)!ftruncate(fd, 8192);
    M("fallocate"); fallocate(fd, FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE, 0, 4096);
    M("posix_fadvise"); posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM);
    M("lseek"); lseek(fd, 0, SEEK_SET);
    M("read"); (void)!read(fd, buf, 16);
    M("write"); (void)!write(fd, buf, 16);
    M("pread"); (void)!pread(fd, buf, 16, 0);
    M("pwrite"); (void)!pwrite(fd, buf, 16, 0);
    M("readv"); { struct iovec v = {buf, 16}; (void)!readv(fd, &v, 1); }
    M("writev"); { struct iovec v = {buf, 16}; (void)!writev(fd, &v, 1); }
    M("preadv"); { struct iovec v = {buf, 16}; (void)!preadv(fd, &v, 1, 0); }
    M("pwritev"); { struct iovec v = {buf, 16}; (void)!pwritev(fd, &v, 1, 0); }
    M("fsync"); fsync(fd);
    M("fdatasync"); fdatasync(fd);
    M("flock"); flock(fd, LOCK_EX); flock(fd, LOCK_UN);
    M("fcntl_getfl"); fcntl(fd, F_GETFL);
    M("fcntl_dupfd_cloexec"); close(fcntl(fd, F_DUPFD_CLOEXEC, 0));
    M("ioctl_fionbio"); { int one = 1; ioctl(fd, FIONBIO, &one); }
    M("isatty"); isatty(fd);
    M("opendir_readdir"); { DIR *dp = opendir("."); while (readdir(dp)) {} closedir(dp); }
    M("getcwd"); (void)!getcwd(buf, sizeof buf);
    M("uname"); { struct utsname u; uname(&u); }
    M("getrlimit"); { struct rlimit r; getrlimit(RLIMIT_NOFILE, &r); }
    M("setrlimit"); { struct rlimit r; getrlimit(RLIMIT_NOFILE, &r); setrlimit(RLIMIT_NOFILE, &r); }
    M("prlimit"); { struct rlimit r; prlimit(0, RLIMIT_NOFILE, NULL, &r); }
    M("mmap"); { void *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      M("mprotect"); mprotect(p, 4096, PROT_READ);
      M("madvise"); madvise(p, 8192, MADV_DONTNEED);
      M("mremap"); p = mremap(p, 8192, 16384, MREMAP_MAYMOVE);
      M("munmap"); munmap(p, 16384); }
    M("memfd_create"); close(memfd_create("x", MFD_CLOEXEC));
    M("eventfd"); close(eventfd(0, EFD_NONBLOCK));
    M("timerfd_create"); { int t = timerfd_create(CLOCK_MONOTONIC, 0); struct itimerspec it = {{0, 0}, {0, 0}};
      M("timerfd_settime"); timerfd_settime(t, 0, &it, NULL);
      M("timerfd_gettime"); timerfd_gettime(t, &it); close(t); }
    M("socket"); { int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0); close(s); }
    M("socketpair"); { int sv[2]; socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
      M("send"); send(sv[0], "a", 1, 0);
      M("recv"); recv(sv[1], buf, 1, 0);
      M("sendto"); sendto(sv[0], "a", 1, 0, NULL, 0);
      M("recvfrom"); recvfrom(sv[1], buf, 1, 0, NULL, NULL);
      M("sendmsg"); { struct iovec v = {"a", 1}; struct msghdr m = {0}; m.msg_iov = &v; m.msg_iovlen = 1; sendmsg(sv[0], &m, 0); }
      M("recvmsg"); { struct iovec v = {buf, 1}; struct msghdr m = {0}; m.msg_iov = &v; m.msg_iovlen = 1; recvmsg(sv[1], &m, 0); }
      M("shutdown"); shutdown(sv[0], SHUT_RDWR); close(sv[0]); close(sv[1]); }
    M("bind_listen_accept"); { int s = socket(AF_UNIX, SOCK_STREAM, 0); struct sockaddr_un a = {AF_UNIX}; strcpy(a.sun_path, "sock");
      bind(s, (void *)&a, sizeof a); listen(s, 1);
      int c = socket(AF_UNIX, SOCK_STREAM, 0);
      M("connect"); connect(c, (void *)&a, sizeof a);
      M("accept"); close(accept(s, NULL, NULL));
      M("accept4"); { int c2 = socket(AF_UNIX, SOCK_STREAM, 0); connect(c2, (void *)&a, sizeof a); close(accept4(s, NULL, NULL, SOCK_CLOEXEC)); close(c2); }
      M("getsockopt"); { int v; socklen_t l = sizeof v; getsockopt(c, SOL_SOCKET, SO_TYPE, &v, &l); }
      M("setsockopt"); { int v = 1; setsockopt(c, SOL_SOCKET, SO_PASSCRED, &v, sizeof v); }
      M("getsockname"); { struct sockaddr_un b; socklen_t l = sizeof b; getsockname(c, (void *)&b, &l); }
      close(c); close(s); unlink("sock"); }
    M("sigaction"); { struct sigaction sa = {0}; sa.sa_handler = on_sig; sigaction(SIGUSR1, &sa, NULL); }
    M("signal"); signal(SIGUSR2, on_sig);
    M("sigprocmask"); { sigset_t s; sigemptyset(&s); sigprocmask(SIG_BLOCK, &s, NULL); }
    M("pthread_sigmask"); { sigset_t s; sigemptyset(&s); pthread_sigmask(SIG_BLOCK, &s, NULL); }
    M("sigaltstack"); { stack_t ss = {0}; ss.ss_flags = SS_DISABLE; sigaltstack(&ss, NULL); }
    M("raise"); raise(SIGUSR1);
    M("kill"); kill(getpid(), SIGUSR1);
    M("pthread_kill"); pthread_kill(pthread_self(), SIGUSR1);
    M("pthread_create_join"); { pthread_t t; pthread_create(&t, NULL, thr, NULL); pthread_join(t, NULL); }
    M("getpid"); (void)getpid();
    M("gettid"); (void)gettid();
    M("getppid"); (void)getppid();
    M("getuid"); (void)getuid();
    M("sched_yield"); sched_yield();
    M("sched_getaffinity"); { cpu_set_t c; sched_getaffinity(0, sizeof c, &c); }
    M("getrandom"); (void)!getentropy(buf, 16);
    M("sysconf_nprocs"); (void)sysconf(_SC_NPROCESSORS_ONLN);
    M("sysconf_pagesize"); (void)sysconf(_SC_PAGESIZE);
    M("umask"); umask(022);
    M("alarm"); alarm(0);
    M("setitimer"); { struct itimerval it = {{0, 0}, {0, 0}}; setitimer(ITIMER_REAL, &it, NULL); }
    M("close"); close(fd);
    unlink("f");
    M("exit"); exit(0);
}
