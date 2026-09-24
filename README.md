# diskdingo

Show physical disks and their partitions in one aligned table: device
name, size, space used, space available, percent used, filesystem type
and mount point. Partitions (and anything stacked on them, such as LVM
volumes or APFS containers) are indented under their disk, lsblk-style.

```
$ diskdingo         # physical disks and partitions
$ diskdingo n       # add network drives and shares (nfs, cifs/smb, sshfs, ...)
$ diskdingo m       # add other filesystem mounts (bind mounts, btrfs subvolumes, ...)
$ diskdingo a       # everything, including tmpfs, proc, snaps, loop devices, disk images
$ diskdingo n m     # combine freely; arguments work in any order
$ diskdingo h       # help
```

Every argument also works with dashes and as a long form:
`-n`/`--network`, `-m`/`--mounts`, `-a`/`--all`, `-h`/`--help`.
One-letter flags can be run together (`-nm`). [USAGE.md](USAGE.md) explains
the columns and each group in detail; `diskdingo h` prints the same.

```
$ diskdingo n
DEVICE                           SIZE    USED   AVAIL  USE%  FSTYPE  MOUNT
sda                            931.5G       -       -     -  -       -
├─sda1                             1G  421.8M  600.2M   42%  vfat    /boot
└─sda2                         930.5G  518.8G  410.5G   56%  btrfs   /
sdd                                7M       -       -     -  -       -
└─sdd1                             7M     97K    6.9M    2%  vfat    /run/media/user1/CIRCUITPY

//better-otter2.lab.net/tank1  215.1G   10.2G  204.9G    5%  cifs    /better-otter2
```

Each extra group (network, other mounts, pseudo filesystems) is separated
by a blank line. `SIZE` is the block device size for device rows and the
filesystem size for mount rows. `USE%` is `used / (used + avail)` rounded
up, as `df` does it. A `?` means the filesystem did not answer within
5 seconds (typically a stuck network mount) or refused the query.

## Installing

```
./build.sh
./deploy.sh              # installs target/release/diskdingo to /usr/bin (sudo if needed)
./deploy.sh ~/.local/bin # or any other directory
```

## What goes where

| Shown with | Contents |
|------------|----------|
| (default)  | Disks from `/sys/block` (Linux) or `diskutil` (macOS), their partitions, LVM/md/APFS volumes on top of them, swap partitions, and filesystems that cannot be tied to one device (ZFS datasets). |
| `n`        | Mounts whose type is a network filesystem (nfs, cifs/smb, afp, webdav, sshfs, rclone, 9p, ceph, gluster, ...) or whose FUSE source looks remote (`//host/share`, `host:/path`, `user@host:`). |
| `m`        | Further mounts of a device already listed (bind mounts, btrfs subvolumes, APFS snapshots) shown as `sda2[/@home]`, plus FUSE filesystems mounted from a local path (mergerfs, ...). |
| `a`        | Kernel plumbing (proc, sysfs, cgroup, devtmpfs, ...), tmpfs, autofs triggers, overlay, squashfs/snaps, anything on a loop device, and mounted disk images. |

## Building

Works on Linux and macOS (Intel and Apple Silicon).

```
./build.sh              # cargo build --release
./target/release/diskdingo
```

Cross-check from Linux for macOS targets (type-check only; linking needs
a macOS SDK):

```
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo check --target aarch64-apple-darwin
```

## How it works

- **Linux** reads `/proc/self/mountinfo` for mounts and walks `/sys/block`
  for disks, partitions and the `slaves/` links that describe device
  stacking. Mounts are matched to devices by major:minor; for filesystems
  that report an anonymous device (btrfs, fuseblk) the source path is
  `stat`ed instead. `/proc/swaps` supplies swap usage.
- **macOS** asks the kernel with `getfsstat(2)` for mounts and parses
  `diskutil list -plist` (and `... physical`, to tell hardware from disk
  images) for the disk tree. APFS containers hang off their physical store
  partition; a snapshot mount such as `/dev/disk3s1s1` is attributed to
  `disk3s1`.
- Space statistics come from `statvfs(3)` (`statfs(2)` on macOS, whose
  `statvfs` still has 32-bit block counts). Every mount is queried on its
  own thread with a shared 5-second deadline, so a hung NFS server cannot
  freeze the listing.
