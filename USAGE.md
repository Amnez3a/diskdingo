# diskdingo usage

`diskdingo` prints the storage attached to a machine as one aligned table.
With no arguments it lists physical disks and their partitions; flags add
network shares, other filesystem mounts, or everything.

```
usage: diskdingo [n] [m] [a] [h]
  (none)       physical disks and their partitions
  n, network   add network drives and shares
  m, mounts    add other filesystem mounts (bind mounts, subvolumes, ...)
  a, all       show everything, including pseudo filesystems
  h, help      show this help
```

Arguments work in any order, with or without leading dashes, as a single
letter or a long word: `n`, `-n`, `--network`. Single letters can be run
together: `-nm` is the same as `n m`. `a` implies `n` and `m`.

The same text, including the group descriptions below, is printed by
`diskdingo h`.

## Columns

| Column | Meaning |
|--------|---------|
| DEVICE | Kernel device name (`sda1`, `nvme0n1p2`, `vg-root`, `disk3s1`). Partitions and anything stacked on them are indented under their disk, lsblk-style. Mount rows show the mount source instead: `//nas/share`, `nas:/export`, `sda2[/@home]`. |
| SIZE   | Block device size on device rows; filesystem size on mount rows. |
| USED   | Space in use on the mounted filesystem. |
| AVAIL  | Space available to unprivileged users, as `df` reports it. |
| USE%   | `used / (used + avail)`, rounded up, as `df` computes it. |
| FSTYPE | Filesystem type from the kernel; `swap` for active swap partitions. |
| MOUNT  | Mount point; `[SWAP]` for swap; `-` when not mounted. |

Sizes are 1024-based with a single-letter unit (`7M`, `931.5G`), the same
style as `lsblk` and `df -h`.

A `?` in the size columns means the filesystem did not answer within
5 seconds (typically a stuck network mount) or refused the query (for
example a portal or sandbox mount belonging to another user).

## What each flag adds

Every group is separated from the previous one by a blank line. The
header is printed once.

### Default: disks and partitions

- Disks from `/sys/block` on Linux or `diskutil list` on macOS.
- Their partitions, in partition-number order.
- Devices stacked on a partition: LVM logical volumes and md arrays on
  Linux (shown by their mapper name), APFS containers and their volumes
  on macOS.
- Swap partitions, with usage from `/proc/swaps`.
- Filesystems that cannot be tied to a single device node, such as ZFS
  datasets, listed flat after the tree.

Only one mount is shown per device: the one with the shortest mount
point, so `/` wins over `/home` for a btrfs filesystem. The others are
available with `m`.

### `n`: network drives and shares

Mounts whose filesystem type is a network filesystem: nfs, cifs/smb,
afp, webdav, sshfs, rclone, s3fs, 9p, ceph, gluster, lustre and similar.
A FUSE mount of an unknown type also counts as network when its source
looks remote: `//host/share`, `scheme://...`, `host:/path` or
`user@host:`.

### `m`: other filesystem mounts

- Further mounts of a device already listed: bind mounts, btrfs
  subvolumes, ZFS child datasets, APFS snapshots. These show the mounted
  subpath after the device, as `findmnt` does: `sda2[/@home]`.
- FUSE filesystems built over a local path rather than a device or a
  server, such as mergerfs.

These are real filesystems with real space, but they duplicate numbers
already visible on a device row, so they are off by default.

### `a`: everything

Adds the mounts that are not storage at all:

- Kernel plumbing: proc, sysfs, cgroup, devtmpfs, devpts, debugfs,
  efivarfs, securityfs and friends.
- tmpfs and ramfs, including `/tmp`, `/run` and `/dev/shm`.
- autofs trigger points, overlay filesystems (containers), snaps and
  other squashfs images, and anything mounted from a loop device.
- On macOS, volumes on mounted disk images (anything `diskutil` does not
  list as physical).

## Examples

```
$ diskdingo
DEVICE    SIZE    USED   AVAIL  USE%  FSTYPE  MOUNT
sda     931.5G       -       -     -  -       -
├─sda1      1G  421.8M  600.2M   42%  vfat    /boot
└─sda2  930.5G  518.8G  410.5G   56%  btrfs   /
sdd         7M       -       -     -  -       -
└─sdd1      7M     97K    6.9M    2%  vfat    /run/media/user1/CIRCUITPY
```

```
$ diskdingo -nm
DEVICE                           SIZE    USED   AVAIL  USE%  FSTYPE  MOUNT
sda                            931.5G       -       -     -  -       -
├─sda1                             1G  421.8M  600.2M   42%  vfat    /boot
└─sda2                         930.5G  518.8G  410.5G   56%  btrfs   /
sdd                                7M       -       -     -  -       -
└─sdd1                             7M     97K    6.9M    2%  vfat    /run/media/user1/CIRCUITPY

//better-otter2.lab.net/tank1  215.1G   10.2G  204.9G    5%  cifs    /better-otter2

sda2[/data1]                   930.5G  518.8G  410.5G   56%  btrfs   /data1
sda2[/@home]                   930.5G  518.8G  410.5G   56%  btrfs   /home
sda2[/@log]                    930.5G  518.8G  410.5G   56%  btrfs   /var/log
```

Output is plain text with two spaces between columns, so it pipes well:

```
$ diskdingo a | grep tmpfs
```

## Exit status

| Code | Meaning |
|------|---------|
| 0    | Listing printed (also when the reader closed the pipe early). |
| 1    | The mount table could not be read. |
| 2    | Unknown argument; usage is printed to stderr. |

## Installing

```
./build.sh               # cargo build --release
./deploy.sh              # install to /usr/bin (sudo if needed)
./deploy.sh ~/.local/bin # or any other directory
```

See [README.md](README.md) for how the tool gathers its data on each
platform.
