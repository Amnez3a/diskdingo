//! diskdingo: physical disks and partitions with size and usage, plus
//! network shares (-n), other filesystem mounts (-m), or everything (-a).

mod devices;
mod mounts;
mod usage;

use devices::{Device, Inventory, Swap};
use mounts::{Class, Mount};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Duration;
use usage::Usage;

/// How long to wait for space statistics before giving up on a mount.
const STAT_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "\
usage: diskdingo [n] [m] [a] [u] [h]
  (none)       physical disks and their partitions
  n, network   add network drives and shares
  m, mounts    add other filesystem mounts (bind mounts, subvolumes, ...)
  a, all       show everything, including pseudo filesystems
  u, uuid      add an ID column with each device's persistent identifier
  h, help      show this help
Arguments may be given in any order, with or without leading dashes,
e.g. `diskdingo n m`, `diskdingo -nmu`, `diskdingo --network --mounts`.

Columns: DEVICE, SIZE, USED, AVAIL, USE%, FSTYPE, MOUNT (and ID with u).
Partitions and anything stacked on them (LVM, md, APFS containers and
volumes) nest under their disk, lsblk-style. SIZE is the block device
size on device rows and the filesystem size on mount rows. USE% is
used / (used + avail) rounded up, as df does it. `?` means the filesystem
did not answer within 5 seconds (a stuck network mount) or refused the
query.

What goes where:
  (none)  disks from /sys/block (Linux), diskutil (macOS) or
          \\\\.\\PhysicalDriveN (Windows), their partitions, LVM/md/APFS
          volumes on top of them, swap partitions, and filesystems not
          tied to one device (ZFS datasets)
  n       mounts of a network type (nfs, cifs/smb, afp, webdav, sshfs,
          rclone, 9p, ceph, gluster, ...) or whose FUSE source looks
          remote (//host/share, host:/path, user@host:); mapped drive
          letters on Windows
  m       further mounts of a device already listed, shown as
          sda2[/@home] (bind mounts, btrfs subvolumes, APFS snapshots,
          Windows folder mount points), plus FUSE filesystems mounted
          from a local path (mergerfs, ...) and Windows subst drives
  a       kernel plumbing (proc, sysfs, cgroup, devtmpfs, ...), tmpfs,
          autofs triggers, overlay, squashfs/snaps, anything on a loop
          device, mounted disk images (VHD, DMG) and RAM disks
Each extra group is separated from the previous one by a blank line.

ID column (u): the name to give `zpool create` (or fstab) so it keeps
working when devices are renumbered. Linux: the /dev/disk/by-id path,
preferring the drive's WWN or EUI name (independent of the controller
it is plugged into) over the model+serial name; mount rows show the
filesystem UUID. macOS: the media UUID, which OpenZFS on OS X keys its
/var/run/disk/by-id links on. Windows: the volume GUID path.";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Opts {
    network: bool,
    mounts: bool,
    all: bool,
    ids: bool,
}

fn main() {
    let opts = parse_args(std::env::args().skip(1));
    let mounts = match mounts::list_mounts() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("diskdingo: cannot list mounts: {e}");
            std::process::exit(1);
        }
    };
    let inventory = devices::inventory();
    let swaps = devices::swaps();
    let sections = build_sections(opts, &mounts, &inventory, &swaps);

    let paths: Vec<String> = sections
        .iter()
        .flatten()
        .filter_map(|r| match &r.stat {
            Stat::Path(p) => Some(p.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let usages = if paths.is_empty() {
        HashMap::new()
    } else {
        usage::stat_all(&paths, STAT_TIMEOUT)
    };

    // Write through a BufWriter and stop quietly if stdout goes away
    // (e.g. `diskdingo a | head`), instead of panicking on EPIPE.
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if render(&mut out, &sections, &usages, opts.ids).is_err() {
        std::process::exit(0);
    }
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Opts {
    let mut o = Opts::default();
    for arg in args {
        let word = arg.trim_start_matches('-');
        // `-nm` / `nm`: several one-letter flags run together.
        let flags: Vec<String> = if word.len() > 1 && word.chars().all(|c| "nmau".contains(c)) {
            word.chars().map(String::from).collect()
        } else {
            vec![word.to_string()]
        };
        for f in flags {
            match f.as_str() {
                "n" | "network" => o.network = true,
                "m" | "mounts" => o.mounts = true,
                "a" | "all" => o.all = true,
                "u" | "uuid" | "id" => o.ids = true,
                "h" | "help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                _ => {
                    eprintln!("unknown argument: {arg}\n{USAGE}");
                    std::process::exit(2);
                }
            }
        }
    }
    o
}

// ---------------------------------------------------------------------------
// Classifying mounts

const NETWORK_TYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "afpfs",
    "afp",
    "webdav",
    "davfs",
    "fuse.davfs2",
    "sshfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.s3fs",
    "s3fs",
    "fuse.gcsfuse",
    "fuse.juicefs",
    "fuse.glusterfs",
    "glusterfs",
    "ceph",
    "fuse.ceph",
    "fuse.ceph-fuse",
    "9p",
    "afs",
    "ncpfs",
    "ncp",
    "coda",
    "lustre",
    "gpfs",
    "fuse.curlftpfs",
    "ftp",
];

const PSEUDO_TYPES: &[&str] = &[
    "proc",
    "procfs",
    "sysfs",
    "devtmpfs",
    "devfs",
    "devpts",
    "fdesc",
    "tmpfs",
    "ramfs",
    "cgroup",
    "cgroup2",
    "pstore",
    "bpf",
    "autofs",
    "debugfs",
    "tracefs",
    "hugetlbfs",
    "configfs",
    "mqueue",
    "fusectl",
    "binfmt_misc",
    "securityfs",
    "efivarfs",
    "selinuxfs",
    "nsfs",
    "rpc_pipefs",
    "sunrpc",
    "squashfs",
    "overlay",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fuse.lxcfs",
    "fuse.snapfuse",
    "fuse.squashfuse",
    "lifs",
    "kernfs",
    "none",
];

fn is_network(m: &Mount) -> bool {
    let t = m.fstype.as_str();
    if NETWORK_TYPES.contains(&t) {
        return true;
    }
    if PSEUDO_TYPES.contains(&t) || m.source.starts_with('/') {
        return false;
    }
    // Unknown (typically FUSE) type whose source looks remote:
    // //host/share, scheme://..., host:/export, user@host:dir
    m.source.starts_with("//")
        || m.source.contains("://")
        || m.source.split_once(':').is_some_and(|(host, path)| {
            !host.is_empty() && !host.contains('/') && (path.starts_with('/') || host.contains('@'))
        })
}

fn classify(m: &Mount, inv: &Inventory) -> Class {
    if let Some(c) = m.class {
        return c;
    }
    if is_network(m) {
        return Class::Network;
    }
    if inv.is_virtual_source(&m.source, m.blkdev) || PSEUDO_TYPES.contains(&m.fstype.as_str()) {
        return Class::Pseudo;
    }
    // ZFS datasets and multi-device filesystems are real storage even
    // though they cannot be pinned to one device node.
    if m.fstype == "zfs" || m.source.starts_with("/dev/") {
        return Class::Block;
    }
    if m.source.starts_with('/') || m.fstype == "drvfs" {
        return Class::Other;
    }
    Class::Pseudo
}

/// Is this mount's source the given device? Linux compares device numbers;
/// elsewhere node paths are compared, also accepting an APFS snapshot of
/// the device (`/dev/disk3s1s1` is a snapshot of `disk3s1`).
fn device_matches(d: &Device, m: &Mount) -> bool {
    match (d.dev, m.blkdev) {
        (Some(a), Some(b)) => a == b,
        _ => {
            m.source == d.path
                || m.source
                    .strip_prefix(&d.path)
                    .and_then(|rest| rest.strip_prefix('s'))
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        }
    }
}

// ---------------------------------------------------------------------------
// Building rows

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stat {
    None,
    /// Ask the kernel about the filesystem mounted here.
    Path(String),
    /// Already known (swap areas).
    Fixed {
        used: u64,
        avail: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    device: String,
    /// Block device size; falls back to the filesystem size when unknown.
    size: Option<u64>,
    fstype: String,
    mount: String,
    stat: Stat,
    id: Option<String>,
}

impl Row {
    fn for_mount(label: String, m: &Mount, inv: &Inventory) -> Row {
        Row {
            device: label,
            size: None,
            fstype: m.fstype.clone(),
            mount: m.target.clone(),
            stat: Stat::Path(m.target.clone()),
            id: inv.mount_id(m),
        }
    }
}

/// Source without `/dev/`, plus the mounted subpath for binds/subvolumes:
/// `sda2[/@home]`. Platform code may supply a label instead.
fn mount_label(m: &Mount) -> String {
    let base = m.label.clone().unwrap_or_else(|| {
        m.source
            .strip_prefix("/dev/")
            .unwrap_or(&m.source)
            .to_string()
    });
    if m.root.is_empty() || m.root == "/" {
        base
    } else {
        format!("{base}[{}]", m.root)
    }
}

struct Builder<'a> {
    mounts: &'a [Mount],
    classes: Vec<Class>,
    swaps: &'a [Swap],
    /// Mounts shown on a device row.
    primary: HashSet<usize>,
    /// Further mounts of a device already shown (subvolumes, bind mounts).
    secondary: HashSet<usize>,
    rows: Vec<Row>,
}

impl Builder<'_> {
    /// Emit a row for `d` and, indented lsblk-style, for everything below it.
    fn walk(&mut self, d: &Device, prefix: &str, last: Option<bool>) {
        let label = match last {
            None => d.name.clone(),
            Some(l) => format!("{prefix}{}{}", if l { "└─" } else { "├─" }, d.name),
        };
        let mut mine: Vec<usize> = (0..self.mounts.len())
            .filter(|&i| self.classes[i] == Class::Block && device_matches(d, &self.mounts[i]))
            .collect();
        // The shortest mount point is the "main" one (/ before /home).
        mine.sort_by_key(|&i| (self.mounts[i].target.len(), i));

        let mut row = Row {
            device: label,
            size: d.size,
            fstype: "-".into(),
            mount: "-".into(),
            stat: Stat::None,
            id: d.id.clone(),
        };
        if let Some((&p, rest)) = mine.split_first() {
            let m = &self.mounts[p];
            row.fstype = m.fstype.clone();
            row.mount = m.target.clone();
            row.stat = Stat::Path(m.target.clone());
            self.primary.insert(p);
            self.secondary.extend(rest.iter().copied());
        } else if let Some(s) = self.swaps.iter().find(|s| Some(s.dev) == d.dev) {
            row.fstype = "swap".into();
            row.mount = "[SWAP]".into();
            row.stat = Stat::Fixed {
                used: s.used,
                avail: s.size.saturating_sub(s.used),
            };
        }
        self.rows.push(row);

        let child_prefix = match last {
            None => String::new(),
            Some(l) => format!("{prefix}{}", if l { "  " } else { "│ " }),
        };
        let n = d.children.len();
        for (i, c) in d.children.iter().enumerate() {
            self.walk(c, &child_prefix, Some(i + 1 == n));
        }
    }
}

/// Rows grouped into sections: devices, network, other, pseudo. Empty
/// sections are dropped; a blank line separates the rest.
fn build_sections(opts: Opts, mounts: &[Mount], inv: &Inventory, swaps: &[Swap]) -> Vec<Vec<Row>> {
    let mut b = Builder {
        mounts,
        classes: mounts.iter().map(|m| classify(m, inv)).collect(),
        swaps,
        primary: HashSet::new(),
        secondary: HashSet::new(),
        rows: Vec::new(),
    };
    for d in &inv.tree {
        b.walk(d, "", None);
    }
    let by_target = |a: &usize, c: &usize| mounts[*a].target.cmp(&mounts[*c].target);
    let labelled = |idx: Vec<usize>| -> Vec<Row> {
        idx.into_iter()
            .map(|i| Row::for_mount(mount_label(&mounts[i]), &mounts[i], inv))
            .collect()
    };

    // Block filesystems with no device row to sit on (ZFS datasets, disks
    // hidden from sysfs inside a container, diskutil failing) go in flat.
    let mut devices = b.rows;
    let mut unmatched: Vec<usize> = (0..mounts.len())
        .filter(|i| {
            b.classes[*i] == Class::Block && !b.primary.contains(i) && !b.secondary.contains(i)
        })
        .collect();
    unmatched.sort_by(by_target);
    devices.extend(labelled(unmatched));

    let mut sections = vec![devices];
    if opts.network || opts.all {
        let mut idx: Vec<usize> = (0..mounts.len())
            .filter(|i| b.classes[*i] == Class::Network)
            .collect();
        idx.sort_by(by_target);
        sections.push(
            idx.into_iter()
                .map(|i| Row::for_mount(mounts[i].source.clone(), &mounts[i], inv))
                .collect(),
        );
    }
    if opts.mounts || opts.all {
        let mut idx: Vec<usize> = (0..mounts.len())
            .filter(|i| b.classes[*i] == Class::Other || b.secondary.contains(i))
            .collect();
        idx.sort_by(by_target);
        sections.push(labelled(idx));
    }
    if opts.all {
        let mut idx: Vec<usize> = (0..mounts.len())
            .filter(|i| b.classes[*i] == Class::Pseudo)
            .collect();
        idx.sort_by(by_target);
        sections.push(labelled(idx));
    }
    sections.retain(|s| !s.is_empty());
    sections
}

// ---------------------------------------------------------------------------
// Output

const HEADER: [&str; 8] = [
    "DEVICE", "SIZE", "USED", "AVAIL", "USE%", "FSTYPE", "MOUNT", "ID",
];
/// Columns that are right-aligned (the numeric ones).
const RIGHT: [bool; 8] = [false, true, true, true, true, false, false, false];

/// 1024-based size like lsblk/df -h: 7M, 931.5G, 4G.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        return format!("{bytes}B");
    }
    let s = format!("{v:.1}");
    format!("{}{}", s.strip_suffix(".0").unwrap_or(&s), UNITS[i])
}

/// Percentage used the way df computes it: used / (used + avail), rounded up.
fn percent(used: u64, avail: u64) -> String {
    let total = u128::from(used) + u128::from(avail);
    if total == 0 {
        return "-".into();
    }
    format!("{}%", (u128::from(used) * 100).div_ceil(total))
}

fn cells(r: &Row, usages: &HashMap<String, Option<Usage>>, ids: bool) -> Vec<String> {
    let dash = || "-".to_string();
    let (fs_size, used, avail, known) = match &r.stat {
        Stat::None => (None, None, None, true),
        Stat::Fixed { used, avail } => (None, Some(*used), Some(*avail), true),
        Stat::Path(p) => match usages.get(p).copied().flatten() {
            Some(u) => (Some(u.size), Some(u.used), Some(u.avail), true),
            None => (None, None, None, false),
        },
    };
    let unknown = || if known { dash() } else { "?".to_string() };
    let mut out = vec![
        r.device.clone(),
        r.size.or(fs_size).map(human).unwrap_or_else(unknown),
        used.map(human).unwrap_or_else(unknown),
        avail.map(human).unwrap_or_else(unknown),
        match (used, avail) {
            (Some(u), Some(a)) => percent(u, a),
            _ => unknown(),
        },
        r.fstype.clone(),
        r.mount.clone(),
    ];
    if ids {
        out.push(r.id.clone().unwrap_or_else(dash));
    }
    out
}

fn render<W: Write>(
    out: &mut W,
    sections: &[Vec<Row>],
    usages: &HashMap<String, Option<Usage>>,
    ids: bool,
) -> std::io::Result<()> {
    let ncols = if ids { HEADER.len() } else { HEADER.len() - 1 };
    let table: Vec<Vec<Vec<String>>> = sections
        .iter()
        .map(|s| s.iter().map(|r| cells(r, usages, ids)).collect())
        .collect();
    let mut widths: Vec<usize> = HEADER[..ncols].iter().map(|h| h.len()).collect();
    for row in table.iter().flatten() {
        for (c, v) in row.iter().enumerate() {
            widths[c] = widths[c].max(v.chars().count());
        }
    }
    let line = |cols: &[&str]| -> String {
        let s: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(c, v)| {
                let pad = widths[c].saturating_sub(v.chars().count());
                if RIGHT[c] {
                    format!("{}{v}", " ".repeat(pad))
                } else {
                    format!("{v}{}", " ".repeat(pad))
                }
            })
            .collect();
        s.join("  ").trim_end().to_string()
    };
    writeln!(out, "{}", line(&HEADER[..ncols]))?;
    for (i, section) in table.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        for row in section {
            let cols: Vec<&str> = row.iter().map(String::as_str).collect();
            writeln!(out, "{}", line(&cols))?;
        }
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(source: &str, target: &str, fstype: &str) -> Mount {
        Mount::new(source, target, fstype)
    }

    fn args(list: &[&str]) -> Opts {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn arguments_in_any_form() {
        assert_eq!(args(&[]), Opts::default());
        assert_eq!(
            args(&["n"]),
            Opts {
                network: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["-m", "--network"]),
            Opts {
                network: true,
                mounts: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["-nm"]),
            Opts {
                network: true,
                mounts: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["am"]),
            Opts {
                mounts: true,
                all: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["--all"]),
            Opts {
                all: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["-u"]),
            Opts {
                ids: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["--uuid", "n"]),
            Opts {
                ids: true,
                network: true,
                ..Opts::default()
            }
        );
        assert_eq!(
            args(&["-nmau"]),
            Opts {
                network: true,
                mounts: true,
                all: true,
                ids: true
            }
        );
    }

    #[test]
    fn classifies_mounts() {
        let inv = Inventory {
            virtual_paths: vec!["/dev/loop3".into()],
            ..Inventory::default()
        };
        let cases = [
            (mount("/dev/sda1", "/boot", "vfat"), Class::Block),
            (mount("/dev/mapper/vg-root", "/", "ext4"), Class::Block),
            (mount("/dev/disk3s1s1", "/", "apfs"), Class::Block),
            (mount("rpool/ROOT/ubuntu", "/", "zfs"), Class::Block),
            (mount("//nas/share", "/mnt/nas", "cifs"), Class::Network),
            (mount("nas:/export", "/mnt/nfs", "nfs4"), Class::Network),
            (
                mount("user@host:/srv", "/mnt/ssh", "fuse.sshfs"),
                Class::Network,
            ),
            (mount("box:", "/mnt/rc", "fuse.rclone"), Class::Network),
            (
                mount("//user@server/vol", "/Volumes/vol", "smbfs"),
                Class::Network,
            ),
            (
                mount("/mnt/a:/mnt/b", "/pool", "fuse.mergerfs"),
                Class::Other,
            ),
            (mount("C:\\", "/mnt/c", "drvfs"), Class::Other),
            (mount("tmpfs", "/tmp", "tmpfs"), Class::Pseudo),
            (
                mount("/dev/loop3", "/snap/core/1", "squashfs"),
                Class::Pseudo,
            ),
            (
                mount("gvfsd-fuse", "/run/user/1000/gvfs", "fuse.gvfsd-fuse"),
                Class::Pseudo,
            ),
            (
                mount("portal", "/run/user/1000/doc", "fuse.portal"),
                Class::Pseudo,
            ),
            (mount("systemd-1", "/mnt/auto", "autofs"), Class::Pseudo),
            (
                mount("overlay", "/var/lib/docker/overlay2/x/merged", "overlay"),
                Class::Pseudo,
            ),
            (
                mount("map auto_home", "/System/Volumes/Data/home", "autofs"),
                Class::Pseudo,
            ),
            (mount("devfs", "/dev", "devfs"), Class::Pseudo),
        ];
        for (m, want) in cases {
            assert_eq!(
                classify(&m, &inv),
                want,
                "{} on {} ({})",
                m.source,
                m.target,
                m.fstype
            );
        }
        // A platform-supplied class wins over the heuristics.
        let win = Mount {
            class: Some(Class::Network),
            ..mount("\\\\nas\\share", "Z:\\", "remote")
        };
        assert_eq!(classify(&win, &inv), Class::Network);
    }

    #[test]
    fn matches_devices_by_number_or_path() {
        let sda1 = Device {
            name: "sda1".into(),
            path: "/dev/sda1".into(),
            dev: Some((8, 1)),
            ..Device::default()
        };
        let mut m = mount("/dev/disk/by-uuid/abcd", "/boot", "vfat");
        m.blkdev = Some((8, 1));
        assert!(device_matches(&sda1, &m));
        m.blkdev = Some((8, 2));
        assert!(!device_matches(&sda1, &m));

        let vol = Device {
            name: "disk3s1".into(),
            path: "/dev/disk3s1".into(),
            ..Device::default()
        };
        assert!(device_matches(&vol, &mount("/dev/disk3s1", "/x", "apfs")));
        assert!(
            device_matches(&vol, &mount("/dev/disk3s1s1", "/", "apfs")),
            "snapshot of the volume"
        );
        assert!(!device_matches(&vol, &mount("/dev/disk3s10", "/y", "apfs")));
        let disk = Device {
            name: "disk3".into(),
            path: "/dev/disk3".into(),
            ..Device::default()
        };
        assert!(!device_matches(
            &disk,
            &mount("/dev/disk3s1s1", "/", "apfs")
        ));

        let guid = "\\\\?\\Volume{1234}\\";
        let win = Device {
            name: "Partition2".into(),
            path: guid.into(),
            ..Device::default()
        };
        assert!(device_matches(&win, &mount(guid, "C:\\", "NTFS")));
    }

    #[test]
    fn sections_tree_and_flags() {
        let sda = Device {
            name: "sda".into(),
            path: "/dev/sda".into(),
            size: Some(1 << 40),
            dev: Some((8, 0)),
            id: Some("/dev/disk/by-id/wwn-0x1".into()),
            children: vec![
                Device {
                    name: "sda1".into(),
                    path: "/dev/sda1".into(),
                    size: Some(1 << 30),
                    dev: Some((8, 1)),
                    ..Device::default()
                },
                Device {
                    name: "sda2".into(),
                    path: "/dev/sda2".into(),
                    size: Some(1 << 39),
                    dev: Some((8, 2)),
                    id: Some("/dev/disk/by-id/wwn-0x1-part2".into()),
                    ..Device::default()
                },
            ],
        };
        let inv = Inventory {
            tree: vec![sda],
            fs_uuids: HashMap::from([((8, 2), "9c0e-fs-uuid".to_string())]),
            ..Inventory::default()
        };
        let mut root = mount("/dev/sda2", "/", "btrfs");
        root.blkdev = Some((8, 2));
        root.root = "/@".into();
        let mut home = mount("/dev/sda2", "/home", "btrfs");
        home.blkdev = Some((8, 2));
        home.root = "/@home".into();
        let mut boot = mount("/dev/sda1", "/boot", "vfat");
        boot.blkdev = Some((8, 1));
        let mounts = vec![
            home,
            root,
            boot,
            mount("//nas/share", "/mnt/nas", "cifs"),
            mount("tmpfs", "/tmp", "tmpfs"),
        ];
        let swaps = vec![Swap {
            dev: (8, 1),
            size: 10,
            used: 5,
        }];

        let s = build_sections(Opts::default(), &mounts, &inv, &swaps);
        assert_eq!(s.len(), 1);
        let devs: Vec<(&str, &str)> = s[0]
            .iter()
            .map(|r| (r.device.as_str(), r.mount.as_str()))
            .collect();
        assert_eq!(devs, [("sda", "-"), ("├─sda1", "/boot"), ("└─sda2", "/")]);
        assert_eq!(
            s[0][1].stat,
            Stat::Path("/boot".into()),
            "a mounted partition is not reported as swap"
        );
        assert_eq!(s[0][0].id.as_deref(), Some("/dev/disk/by-id/wwn-0x1"));
        assert_eq!(s[0][1].id, None);
        assert_eq!(s[0][2].id.as_deref(), Some("/dev/disk/by-id/wwn-0x1-part2"));

        let s = build_sections(
            Opts {
                network: true,
                mounts: true,
                ..Opts::default()
            },
            &mounts,
            &inv,
            &swaps,
        );
        assert_eq!(s.len(), 3);
        assert_eq!(s[1][0].device, "//nas/share");
        assert_eq!(s[1][0].id, None);
        assert_eq!(
            (s[2][0].device.as_str(), s[2][0].mount.as_str()),
            ("sda2[/@home]", "/home")
        );
        assert_eq!(
            s[2][0].id.as_deref(),
            Some("9c0e-fs-uuid"),
            "mount rows carry the filesystem UUID"
        );

        let s = build_sections(
            Opts {
                all: true,
                ..Opts::default()
            },
            &mounts,
            &inv,
            &swaps,
        );
        assert_eq!(s.len(), 4);
        assert_eq!(s[3][0].mount, "/tmp");
    }

    #[test]
    fn windows_style_rows() {
        let guid = "\\\\?\\Volume{abcd}\\";
        let disk = Device {
            name: "PhysicalDrive0".into(),
            path: "\\\\.\\PhysicalDrive0".into(),
            size: Some(1 << 40),
            children: vec![Device {
                name: "Partition3".into(),
                path: guid.into(),
                size: Some(1 << 39),
                id: Some(guid.into()),
                ..Device::default()
            }],
            ..Device::default()
        };
        let inv = Inventory {
            tree: vec![disk],
            ..Inventory::default()
        };
        let mounts = vec![
            Mount {
                label: Some("Harddisk0Partition3".into()),
                class: Some(Class::Block),
                ..mount(guid, "C:\\", "NTFS")
            },
            Mount {
                label: Some("Harddisk0Partition3".into()),
                class: Some(Class::Block),
                ..mount(guid, "C:\\mnt\\data", "NTFS")
            },
            Mount {
                class: Some(Class::Network),
                ..mount("\\\\nas\\share", "Z:\\", "remote")
            },
            Mount {
                class: Some(Class::Other),
                ..mount("C:\\Users\\me\\proj", "P:\\", "NTFS")
            },
        ];
        let s = build_sections(
            Opts {
                network: true,
                mounts: true,
                ids: true,
                ..Opts::default()
            },
            &mounts,
            &inv,
            &[],
        );
        assert_eq!(s.len(), 3);
        assert_eq!(
            (
                s[0][1].device.as_str(),
                s[0][1].mount.as_str(),
                s[0][1].id.as_deref()
            ),
            ("└─Partition3", "C:\\", Some(guid))
        );
        assert_eq!(
            (s[1][0].device.as_str(), s[1][0].mount.as_str()),
            ("\\\\nas\\share", "Z:\\")
        );
        let other: Vec<(&str, &str)> = s[2]
            .iter()
            .map(|r| (r.device.as_str(), r.mount.as_str()))
            .collect();
        assert_eq!(
            other,
            [
                ("Harddisk0Partition3", "C:\\mnt\\data"),
                ("C:\\Users\\me\\proj", "P:\\")
            ]
        );
        assert_eq!(
            s[2][0].id.as_deref(),
            Some(guid),
            "volume rows show the volume GUID path"
        );
    }

    #[test]
    fn unmounted_swap_partition() {
        let sdb1 = Device {
            name: "sdb1".into(),
            path: "/dev/sdb1".into(),
            size: Some(1 << 30),
            dev: Some((8, 17)),
            ..Device::default()
        };
        let inv = Inventory {
            tree: vec![sdb1],
            ..Inventory::default()
        };
        let s = build_sections(
            Opts::default(),
            &[],
            &inv,
            &[Swap {
                dev: (8, 17),
                size: 100,
                used: 30,
            }],
        );
        assert_eq!(s[0][0].fstype, "swap");
        assert_eq!(
            s[0][0].stat,
            Stat::Fixed {
                used: 30,
                avail: 70
            }
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1024), "1K");
        assert_eq!(human(7340544), "7M");
        assert_eq!(human(1953525168 * 512), "931.5G");
        assert_eq!(human(1 << 40), "1T");
    }

    #[test]
    fn percent_rounds_up_like_df() {
        assert_eq!(percent(0, 0), "-");
        assert_eq!(percent(1, 99), "1%");
        assert_eq!(percent(1, 999), "1%");
        assert_eq!(percent(50, 50), "50%");
        assert_eq!(percent(10, 0), "100%");
    }

    #[test]
    fn renders_aligned_table() {
        let rows = vec![vec![
            Row {
                device: "sda".into(),
                size: Some(1 << 30),
                fstype: "-".into(),
                mount: "-".into(),
                stat: Stat::None,
                id: Some("/dev/disk/by-id/wwn-0x1".into()),
            },
            Row {
                device: "└─sda1".into(),
                size: Some(1 << 20),
                fstype: "ext4".into(),
                mount: "/".into(),
                stat: Stat::Path("/".into()),
                id: None,
            },
        ]];
        let usages = HashMap::from([(
            "/".to_string(),
            Some(Usage {
                size: 1 << 20,
                used: 1 << 19,
                avail: 1 << 19,
            }),
        )]);
        let mut out = Vec::new();
        render(&mut out, &rows, &usages, false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "DEVICE  SIZE  USED  AVAIL  USE%  FSTYPE  MOUNT\n\
             sda       1G     -      -     -  -       -\n\
             └─sda1    1M  512K   512K   50%  ext4    /\n"
        );
        let mut out = Vec::new();
        render(&mut out, &rows, &usages, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "DEVICE  SIZE  USED  AVAIL  USE%  FSTYPE  MOUNT  ID\n\
             sda       1G     -      -     -  -       -      /dev/disk/by-id/wwn-0x1\n\
             └─sda1    1M  512K   512K   50%  ext4    /      -\n"
        );
    }
}
