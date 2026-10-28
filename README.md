# mkfs-sysv

Create SVR4 (System V) filesystem images in Rust.

The on-disk format matches Linux's historical `sysv_fs.h` (SVR4 variant).
Mounting requires a kernel with SysV filesystem support enabled, such as
Linux 6.12 with `CONFIG_SYSV_FS`. Mainline Linux [removed the driver][sysv-removal]
in 6.15; the mount command below will not work on newer mainline kernels.

[sysv-removal]: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=aaca83f7b13fbe54c853f63eca9e849e6b441459

## Install

From crates.io:

```bash
cargo install mkfs-sysv
```

This installs a binary called `mkfs-sysv`. Pre-built binaries for Linux
(x86_64 / aarch64), macOS (universal), and Windows are also attached to
each [GitHub release][releases].

[releases]: https://github.com/bitplane/mkfs-sysv/releases

## Usage

```bash
# Create an empty 4 MB image.
mkfs-sysv out.sysv 4

# Populate from a directory tree (recursive — subdirectories are preserved).
mkfs-sysv -d ./payload out.sysv 4

# Mount on a Linux kernel with SysV support (see above).
sudo mount -o loop,ro -t sysv out.sysv /mnt
```

## Limits

This is a "test image" mkfs:

- Population accepts regular files and directories; symlinks and special files
  are rejected with an error. Unix ownership IDs must fit in 16 bits.
- 256 inodes total (fixed by layout: 32 inode blocks × 8 inodes/block).
- 14-byte filenames; longer names are truncated.
- Files use direct-only block addressing (10 zone pointers), so the
  largest file is 10 × 512 = 5 KB.
- Same upper bound applies to directory listings (~320 entries).

## License

WTFPL with one extra clause: don't blame me. See [LICENSE](LICENSE).
