/*
 * fs_erofs.h — C ABI for the pure-Rust read-only EROFS driver.
 *
 * Link against libfs_erofs.a and #include this header. UTF-8 paths,
 * NULL / -1 failure sentinels with thread-local error detail via
 * fs_erofs_last_error() / fs_erofs_last_errno().
 *
 * EROFS is an inherently read-only filesystem: there is no write surface.
 *
 * MIT License — see LICENSE
 */

#ifndef FS_EROFS_H
#define FS_EROFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a mounted EROFS filesystem. */
typedef struct fs_erofs_fs fs_erofs_fs_t;

/* File type enumeration (matches the directory-entry type byte). */
typedef enum {
    FS_EROFS_FT_UNKNOWN  = 0,
    FS_EROFS_FT_REG_FILE = 1,
    FS_EROFS_FT_DIR      = 2,
    FS_EROFS_FT_CHRDEV   = 3,
    FS_EROFS_FT_BLKDEV   = 4,
    FS_EROFS_FT_FIFO     = 5,
    FS_EROFS_FT_SOCK     = 6,
    FS_EROFS_FT_SYMLINK  = 7,
} fs_erofs_file_type_t;

/* File/directory attributes. `mode` carries permission bits only; combine
 * with the type bits implied by `file_type` to form a full st_mode.
 * `inode` is the 64-bit EROFS NID. */
typedef struct {
    uint64_t inode;
    uint16_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint32_t mtime;
    uint32_t link_count;
    uint32_t file_type;   /* fs_erofs_file_type_t */
} fs_erofs_attr_t;

/* Directory entry (returned during iteration). */
typedef struct {
    uint64_t inode;
    uint8_t  file_type;   /* fs_erofs_file_type_t */
    uint8_t  name_len;
    char     name[256];   /* null-terminated */
} fs_erofs_dirent_t;

/* Volume information snapshotted from the superblock. */
typedef struct {
    uint32_t block_size;
    uint32_t total_blocks;
    uint64_t inode_count;
    uint64_t build_time;       /* unix epoch seconds */
    char     volume_name[16];  /* NUL-terminated, <= 16 bytes */
    uint8_t  uuid[16];         /* raw 16-byte UUID */
    uint32_t feature_compat;
    uint32_t feature_incompat;
} fs_erofs_volume_info_t;

/* ---- Block device callback interface (read-only) ---- */

typedef int (*fs_erofs_read_fn)(void *context, void *buf,
                                uint64_t offset, uint64_t length);

typedef struct {
    fs_erofs_read_fn read;
    void   *context;      /* opaque; e.g. an FSBlockDeviceResource pointer */
    uint64_t size_bytes;  /* total device / partition size */
    uint32_t block_size;  /* physical block size (e.g. 512); informational */
} fs_erofs_blockdev_cfg_t;

/* ---- Lifecycle ---- */

fs_erofs_fs_t *fs_erofs_mount(const char *device_path);
fs_erofs_fs_t *fs_erofs_mount_with_callbacks(const fs_erofs_blockdev_cfg_t *cfg);

/* Mount via an FsCoreDevice handle from a sister crate (e.g.
 * fs_core_device_from_callbacks / fs_core_device_slice_ro from am-fs-core).
 * The handle's refcount is incremented; the caller still owns its
 * *FsCoreDevice and frees it via fs_core_device_close. Forward declared —
 * full definition in fs_core.h. NULL on failure. */
struct FsCoreDevice;
fs_erofs_fs_t *fs_erofs_mount_with_fs_core_device(struct FsCoreDevice *handle);

void fs_erofs_umount(fs_erofs_fs_t *fs);

/* ---- Queries ---- */

int fs_erofs_get_volume_info(fs_erofs_fs_t *fs, fs_erofs_volume_info_t *info);

/* Stat a path (relative to mount root). Symlinks are NOT followed.
 * Returns 0 on success, -1 on failure. */
int fs_erofs_stat(fs_erofs_fs_t *fs, const char *path, fs_erofs_attr_t *attr);

/* ---- Directory listing ---- */

typedef struct fs_erofs_dir_iter fs_erofs_dir_iter_t;

fs_erofs_dir_iter_t *fs_erofs_dir_open(fs_erofs_fs_t *fs, const char *path);
const fs_erofs_dirent_t *fs_erofs_dir_next(fs_erofs_dir_iter_t *iter);
void fs_erofs_dir_close(fs_erofs_dir_iter_t *iter);

/* ---- File / symlink reading ---- */

int64_t fs_erofs_read_file(fs_erofs_fs_t *fs, const char *path,
                           void *buf, uint64_t offset, uint64_t length);
int fs_erofs_readlink(fs_erofs_fs_t *fs, const char *path,
                      char *buf, size_t bufsize);

/* ---- Error reporting ---- */

const char *fs_erofs_last_error(void);
int fs_erofs_last_errno(void);

#ifdef __cplusplus
}
#endif

#endif /* FS_EROFS_H */
