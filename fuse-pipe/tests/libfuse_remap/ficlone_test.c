/*
 * ficlone_test.c - Test FICLONE/FICLONERANGE ioctls through FUSE
 *
 * Compile with musl for static binary:
 *   musl-gcc -static -o ficlone_test ficlone_test.c
 *
 * Usage:
 *   ./ficlone_test <source_file> <dest_file>           # FICLONE (whole file)
 *   ./ficlone_test --range <src> <dst> <off> <len>     # FICLONERANGE (partial)
 *
 * Exit codes:
 *   0 - success
 *   1 - ioctl failed (prints errno)
 *   2 - usage error
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/stat.h>

/* FICLONE ioctl - clone entire file */
#ifndef FICLONE
#define FICLONE _IOW(0x94, 9, int)
#endif

/* FICLONERANGE ioctl - clone range of file */
#ifndef FICLONERANGE
#define FICLONERANGE _IOW(0x94, 13, struct file_clone_range)
struct file_clone_range {
    int64_t src_fd;
    uint64_t src_offset;
    uint64_t src_length;
    uint64_t dest_offset;
};
#endif

static void print_error(int err) {
    fprintf(stderr, "ioctl failed: %s (errno=%d)\n", strerror(err), err);
    if (err == EOPNOTSUPP || err == ENOTTY) {
        fprintf(stderr, "  -> Filesystem does not support reflinks\n");
        fprintf(stderr, "  -> For FUSE: kernel needs FUSE_REMAP_FILE_RANGE support\n");
    } else if (err == EXDEV) {
        fprintf(stderr, "  -> Source and dest are on different filesystems\n");
    } else if (err == ENOSYS) {
        fprintf(stderr, "  -> Not implemented (ENOSYS)\n");
        fprintf(stderr, "  -> Kernel may be missing remap_file_range support\n");
    } else if (err == EINVAL) {
        fprintf(stderr, "  -> Invalid argument (offset/length not block-aligned?)\n");
    }
}

int test_ficlone(const char *src_path, const char *dst_path) {
    int src_fd, dst_fd, ret;
    struct stat st, dst_st;

    src_fd = open(src_path, O_RDONLY);
    if (src_fd < 0) {
        fprintf(stderr, "ERROR: Cannot open source '%s': %s\n", src_path, strerror(errno));
        return 1;
    }

    if (fstat(src_fd, &st) < 0) {
        fprintf(stderr, "ERROR: Cannot stat source: %s\n", strerror(errno));
        close(src_fd);
        return 1;
    }

    dst_fd = open(dst_path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (dst_fd < 0) {
        fprintf(stderr, "ERROR: Cannot create dest '%s': %s\n", dst_path, strerror(errno));
        close(src_fd);
        return 1;
    }

    printf("Test FICLONE (whole file): %s -> %s (%ld bytes)\n",
           src_path, dst_path, (long)st.st_size);

    ret = ioctl(dst_fd, FICLONE, src_fd);
    if (ret < 0) {
        print_error(errno);
        close(src_fd);
        close(dst_fd);
        return 1;
    }

    printf("SUCCESS: FICLONE completed\n");

    /* Verify destination file size matches source */
    if (fstat(dst_fd, &dst_st) == 0) {
        if (dst_st.st_size == st.st_size) {
            printf("  Size verified: %ld bytes\n", (long)dst_st.st_size);
        } else {
            fprintf(stderr, "  FAIL: Size mismatch: src=%ld dst=%ld\n",
                    (long)st.st_size, (long)dst_st.st_size);
            close(src_fd);
            close(dst_fd);
            return 1;
        }
    }

    close(src_fd);
    close(dst_fd);
    return 0;
}

int test_ficlonerange(const char *src_path, const char *dst_path,
                      uint64_t offset, uint64_t length) {
    int src_fd, dst_fd, ret;
    struct stat st, dst_st;
    struct file_clone_range range;

    src_fd = open(src_path, O_RDONLY);
    if (src_fd < 0) {
        fprintf(stderr, "ERROR: Cannot open source '%s': %s\n", src_path, strerror(errno));
        return 1;
    }

    if (fstat(src_fd, &st) < 0) {
        fprintf(stderr, "ERROR: Cannot stat source: %s\n", strerror(errno));
        close(src_fd);
        return 1;
    }

    /* For partial clone, create dest file with enough space */
    dst_fd = open(dst_path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (dst_fd < 0) {
        fprintf(stderr, "ERROR: Cannot create dest '%s': %s\n", dst_path, strerror(errno));
        close(src_fd);
        return 1;
    }

    /* Pre-allocate destination to required size */
    uint64_t expected_size = offset + length;
    if (length == 0) {
        expected_size = st.st_size;  /* len=0 means clone to EOF */
    }
    if (ftruncate(dst_fd, expected_size) < 0) {
        fprintf(stderr, "ERROR: Cannot pre-allocate dest: %s\n", strerror(errno));
        close(src_fd);
        close(dst_fd);
        return 1;
    }

    printf("Test FICLONERANGE: %s -> %s (offset=%lu, len=%lu)\n",
           src_path, dst_path, (unsigned long)offset, (unsigned long)length);

    range.src_fd = src_fd;
    range.src_offset = offset;
    range.src_length = length;
    range.dest_offset = offset;

    ret = ioctl(dst_fd, FICLONERANGE, &range);
    if (ret < 0) {
        print_error(errno);
        close(src_fd);
        close(dst_fd);
        return 1;
    }

    printf("SUCCESS: FICLONERANGE completed\n");

    /* Verify destination file size */
    if (fstat(dst_fd, &dst_st) == 0) {
        printf("  Dest size: %ld bytes\n", (long)dst_st.st_size);
    }

    close(src_fd);
    close(dst_fd);
    return 0;
}

int main(int argc, char *argv[]) {
    if (argc >= 3 && strcmp(argv[1], "--range") == 0) {
        /* FICLONERANGE mode */
        if (argc < 6) {
            fprintf(stderr, "Usage: %s --range <src> <dst> <offset> <length>\n", argv[0]);
            fprintf(stderr, "  offset/length should be block-aligned (4096)\n");
            fprintf(stderr, "  length=0 means clone to end of file\n");
            return 2;
        }
        uint64_t offset = strtoull(argv[4], NULL, 0);
        uint64_t length = strtoull(argv[5], NULL, 0);
        return test_ficlonerange(argv[2], argv[3], offset, length);
    } else if (argc >= 3) {
        /* FICLONE mode (whole file) */
        return test_ficlone(argv[1], argv[2]);
    } else {
        fprintf(stderr, "Usage: %s <source> <dest>                    # FICLONE (whole file)\n", argv[0]);
        fprintf(stderr, "       %s --range <src> <dst> <off> <len>    # FICLONERANGE\n", argv[0]);
        fprintf(stderr, "\nTests FICLONE/FICLONERANGE ioctls (reflink/copy-on-write)\n");
        fprintf(stderr, "Exit 0 = success, 1 = ioctl failed, 2 = usage error\n");
        return 2;
    }
}
