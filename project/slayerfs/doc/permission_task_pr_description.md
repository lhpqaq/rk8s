## Summary

- Persist POSIX permission bits in SlayerFS metadata so `stat` / `getattr` return the same mode that was created or updated.
- Add end-to-end `chmod` support through the metadata, VFS, and FUSE layers, while explicitly rejecting unsupported `chown` requests with `ENOSYS`.
- Strip `setuid`, `setgid`, and `sticky` bits on supported mode update paths, including both `chmod` and FUSE create-time mode handling.

## What Changed

- Added permission persistence and default mode handling for files and directories.
- Wired `chmod` through `MetaStore`, `MetaLayer`, `VFS`, and FUSE `setattr` mode updates.
- Ensured `stat` / `getattr` reflects persisted permission changes.
- Documented supported and unsupported permission behavior in `docs/permissions.md`.
- Added regression coverage for default permissions, mode updates, special-bit stripping, `ENOENT`, and Linux FUSE manual verification.
- Fixed the FUSE create-path mode sanitization so commands like `mkdir -m 1777` no longer preserve unsupported special bits.

## Testing

- All unit tests pass on the current branch.
- `project/slayerfs/tests/scripts/manual_fuse_permissions.sh`

The Linux FUSE manual verification covers:

- default file mode (`644`)
- default directory mode (`755`)
- `chmod` persistence across remount
- `chmod 4755` sanitization to `755`
- missing-path `chmod` returning `ENOENT`
- `mkdir -m 1777` sanitization to `777`
- `chown` returning `ENOSYS` / `Function not implemented`

## Notes

- SlayerFS intentionally does not implement `setuid`, `setgid`, `sticky`, ACLs, or `chown` semantics in this change.
- Unsupported special bits are masked before metadata persistence, keeping behavior aligned with the documented permission model.
