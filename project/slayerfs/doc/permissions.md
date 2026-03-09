# SlayerFS Permission Model

## Overview

SlayerFS persists POSIX-style permission bits in file and directory metadata.
Permissions are stored as part of each inode's `Permission` record and are
returned through `stat` / `getattr` to the FUSE layer.

## Supported Features

| Feature | Status |
|---------|--------|
| Standard permission bits (`rwxrwxrwx`, 0o777) | ✅ Supported |
| `chmod` (mode changes via FUSE `setattr`) | ✅ Supported |
| File type preservation across `chmod` | ✅ Supported |
| Default file permissions (0644) | ✅ Supported |
| Default directory permissions (0755) | ✅ Supported |

## Not Supported

| Feature | Reason |
|---------|--------|
| Setuid bit (0o4000) | Stripped on `chmod`; not enforced |
| Setgid bit (0o2000) | Stripped on `chmod`; not enforced |
| Sticky bit (0o1000) | Stripped on `chmod`; not enforced |
| `chown` (uid/gid changes) | Returns `ENOSYS` from FUSE layer |
| POSIX ACLs | Not implemented |
| umask synchronization | VFS defaults are hard-coded; FUSE layer may apply umask at creation time |

## Default Permissions

- **Files** are created with mode `0o100644` (`-rw-r--r--`).
- **Directories** are created with mode `0o040755` (`drwxr-xr-x`).

When files or directories are created through the FUSE layer (e.g., via
`mkdir` or `create`), the kernel-provided `mode` and `umask` are applied:

```
effective_mode = (mode & 0o777) & !(umask & 0o777)
```

## chmod Behavior

When `chmod` is called (either via the VFS `chmod` method or via a FUSE
`setattr` with the mode field set):

1. **Setuid (0o4000), setgid (0o2000), and sticky (0o1000) bits are stripped.**
   Only the standard `rwxrwxrwx` permission bits (0o777) are persisted.
2. The file type bits in the mode word are preserved automatically.
3. The `ctime` (change time) is updated.

### Example

```text
chmod 4755 /mnt/slayerfs/file.txt
# Resulting mode: 0755 (setuid bit silently removed)
```

## Error Handling

| Condition | Error |
|-----------|-------|
| `chmod` on nonexistent inode | `ENOENT` |
| `chown` via FUSE `setattr` | `ENOSYS` |
| Invalid mode bits (above 0o777) | Silently masked before write |

## Concurrency

Permission changes are atomic within each backend:

- **SQLite/PostgreSQL**: Uses database transactions.
- **etcd**: Uses compare-and-swap with optimistic locking.
- **Redis**: Uses Lua scripts for atomicity.

## Future Work

- `chown` support (uid/gid changes).
- POSIX ACL support.
- Setuid/setgid enforcement if security use-cases arise.
