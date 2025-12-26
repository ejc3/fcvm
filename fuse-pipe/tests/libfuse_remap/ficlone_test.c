/*
 * ficlone_test.c - Test FICLONE ioctl through FUSE
 *
 * Compile with musl for static binary:
 *   musl-gcc -static -o ficlone_test ficlone_test.c
 *
 * Usage:
 *   ./ficlone_test <source_file> <dest_file>
 *
 * Exit codes:
 *   0 - FICLONE succeeded
 *   1 - FICLONE failed (prints errno)
 *   2 - Usage error
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

int main(int argc, char *argv[]) {
    int src_fd, dst_fd;
    int ret;
    struct stat st;

    if (argc < 3) {
        fprintf(stderr, "Usage: %s <source> <dest>\n", argv[0]);
        fprintf(stderr, "\nTests FICLONE ioctl (reflink/copy-on-write clone)\n");
        fprintf(stderr, "Exit 0 = success, 1 = ioctl failed, 2 = usage error\n");
        return 2;
    }

    /* Open source file */
    src_fd = open(argv[1], O_RDONLY);
    if (src_fd < 0) {
        fprintf(stderr, "ERROR: Cannot open source '%s': %s\n", argv[1], strerror(errno));
        return 1;
    }

    /* Get source file size for verification */
    if (fstat(src_fd, &st) < 0) {
        fprintf(stderr, "ERROR: Cannot stat source: %s\n", strerror(errno));
        close(src_fd);
        return 1;
    }

    /* Create/truncate destination file */
    dst_fd = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (dst_fd < 0) {
        fprintf(stderr, "ERROR: Cannot create dest '%s': %s\n", argv[2], strerror(errno));
        close(src_fd);
        return 1;
    }

    /* Try FICLONE (whole file clone) */
    printf("Attempting FICLONE: %s -> %s (%ld bytes)\n", argv[1], argv[2], (long)st.st_size);

    ret = ioctl(dst_fd, FICLONE, src_fd);

    if (ret < 0) {
        int err = errno;
        fprintf(stderr, "FICLONE failed: %s (errno=%d)\n", strerror(err), err);

        if (err == EOPNOTSUPP || err == ENOTTY) {
            fprintf(stderr, "  -> Filesystem does not support reflinks\n");
            fprintf(stderr, "  -> For FUSE: kernel needs FUSE_REMAP_FILE_RANGE support\n");
        } else if (err == EXDEV) {
            fprintf(stderr, "  -> Source and dest are on different filesystems\n");
        } else if (err == ENOSYS) {
            fprintf(stderr, "  -> FICLONE not implemented (ENOSYS)\n");
            fprintf(stderr, "  -> Kernel may be missing remap_file_range support\n");
        }

        close(src_fd);
        close(dst_fd);
        return 1;
    }

    printf("SUCCESS: FICLONE completed\n");

    /* Verify destination file size matches */
    struct stat dst_st;
    if (fstat(dst_fd, &dst_st) == 0) {
        if (dst_st.st_size == st.st_size) {
            printf("  Size verified: %ld bytes\n", (long)dst_st.st_size);
        } else {
            fprintf(stderr, "  WARNING: Size mismatch: src=%ld dst=%ld\n",
                    (long)st.st_size, (long)dst_st.st_size);
        }
    }

    close(src_fd);
    close(dst_fd);

    return 0;
}
