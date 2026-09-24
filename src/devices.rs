//! Block device inventory: physical disks, their partitions, and whatever
//! is stacked on top of them (LVM, md, APFS containers and volumes).
//!
//! Linux walks `/sys/block`; macOS parses `diskutil list -plist`.

use std::cmp::Ordering;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    /// Display name: `sda1`, `vg-root`, `disk0s2`.
    pub name: String,
    /// Device node, used to match mount sources: `/dev/sda1`, `/dev/mapper/vg-root`.
    pub path: String,
    /// Size in bytes, when known.
    pub size: Option<u64>,
    /// major:minor (Linux only).
    pub dev: Option<(u32, u32)>,
    pub children: Vec<Device>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Top-level (physical) disks with their partitions nested below.
    pub tree: Vec<Device>,
    /// Block devices that are not real disks: loop devices, zram, mounted
    /// disk images. Filesystems on them are treated as pseudo mounts.
    pub virtual_devs: Vec<(u32, u32)>,
    pub virtual_paths: Vec<String>,
}

impl Inventory {
    pub fn is_virtual_source(&self, source: &str, blkdev: Option<(u32, u32)>) -> bool {
        blkdev.is_some_and(|d| self.virtual_devs.contains(&d))
            || self.virtual_paths.iter().any(|p| p == source)
    }
}

/// An active swap area on a block device (Linux only; empty elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swap {
    pub dev: (u32, u32),
    pub size: u64,
    pub used: u64,
}

pub fn inventory() -> Inventory {
    #[cfg(target_os = "linux")]
    {
        linux::inventory()
    }
    #[cfg(target_os = "macos")]
    {
        macos::inventory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Inventory::default()
    }
}

pub fn swaps() -> Vec<Swap> {
    #[cfg(target_os = "linux")]
    {
        linux::swaps()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Parse "8:1" into (8, 1).
pub fn parse_devnum(s: &str) -> Option<(u32, u32)> {
    let (major, minor) = s.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Split a Linux `dev_t` into (major, minor).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn devnum_from_rdev(rdev: u64) -> (u32, u32) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// major:minor of the block device at `path` (symlinks followed).
#[cfg(target_os = "linux")]
pub fn blkdev_of_path(path: &str) -> Option<(u32, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(path).ok()?;
    md.file_type()
        .is_block_device()
        .then(|| devnum_from_rdev(md.rdev()))
}

/// Order names so that sda2 < sda10 and sdb < sdaa.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (na, nb) = (take_number(&mut ai), take_number(&mut bi));
                if na != nb {
                    return na.cmp(&nb);
                }
            }
            (Some(x), Some(y)) => {
                ai.next();
                bi.next();
                if x != y {
                    return x.cmp(&y);
                }
            }
        }
    }
}

fn take_number(it: &mut std::iter::Peekable<std::str::Chars>) -> u64 {
    let mut n = 0u64;
    while let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
        n = n.saturating_mul(10).saturating_add(u64::from(d));
        it.next();
    }
    n
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{natural_cmp, parse_devnum, Device, Inventory, Swap};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::Path;

    /// Kernel block devices that are never physical disks.
    const VIRTUAL_PREFIXES: [&str; 4] = ["loop", "ram", "zram", "nbd"];

    struct Entry {
        /// Kernel name (`dm-0`), which is what `slaves/` entries refer to.
        sysname: String,
        dev: Device,
        slaves: Vec<String>,
        parts: Vec<Device>,
    }

    fn read_trim(p: &Path) -> Option<String> {
        fs::read_to_string(p).ok().map(|s| s.trim().to_string())
    }

    /// `size` in sysfs is always in 512-byte sectors, whatever the block size.
    fn read_size(dir: &Path) -> Option<u64> {
        read_trim(&dir.join("size"))?
            .parse::<u64>()
            .ok()?
            .checked_mul(512)
    }

    fn read_dev(dir: &Path) -> Option<(u32, u32)> {
        parse_devnum(&read_trim(&dir.join("dev"))?)
    }

    fn is_virtual(name: &str) -> bool {
        VIRTUAL_PREFIXES.iter().any(|p| {
            name.strip_prefix(p)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        })
    }

    /// Partitions of a whole disk, in partition-number order.
    fn partitions(dir: &Path, disk: &str) -> Vec<Device> {
        let mut parts: Vec<(u32, Device)> = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let pdir = e.path();
            if !name.starts_with(disk) || !pdir.join("partition").exists() {
                continue;
            }
            let num = read_trim(&pdir.join("partition"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            parts.push((
                num,
                Device {
                    path: format!("/dev/{name}"),
                    name,
                    size: read_size(&pdir),
                    dev: read_dev(&pdir),
                    children: Vec::new(),
                },
            ));
        }
        parts.sort_by_key(|(n, _)| *n);
        parts.into_iter().map(|(_, d)| d).collect()
    }

    pub fn inventory() -> Inventory {
        inventory_at(Path::new("/sys/block"))
    }

    pub fn inventory_at(sys_block: &Path) -> Inventory {
        let mut inv = Inventory::default();
        let Ok(rd) = fs::read_dir(sys_block) else {
            return inv;
        };
        let mut entries: Vec<Entry> = Vec::new();
        for e in rd.flatten() {
            let sysname = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            let dev = read_dev(&dir);
            let parts = partitions(&dir, &sysname);
            if is_virtual(&sysname) {
                inv.virtual_devs
                    .extend(dev.into_iter().chain(parts.iter().filter_map(|p| p.dev)));
                inv.virtual_paths.push(format!("/dev/{sysname}"));
                inv.virtual_paths.extend(parts.into_iter().map(|p| p.path));
                continue;
            }
            // Device-mapper devices are better known by their mapped name.
            let (name, path) = match read_trim(&dir.join("dm/name")) {
                Some(n) if !n.is_empty() => (n.clone(), format!("/dev/mapper/{n}")),
                _ => (sysname.clone(), format!("/dev/{sysname}")),
            };
            let slaves = fs::read_dir(dir.join("slaves"))
                .map(|r| {
                    r.flatten()
                        .map(|s| s.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            entries.push(Entry {
                sysname,
                dev: Device {
                    name,
                    path,
                    size: read_size(&dir),
                    dev,
                    children: Vec::new(),
                },
                slaves,
                parts,
            });
        }
        entries.sort_by(|a, b| natural_cmp(&a.sysname, &b.sysname));

        // Which entries are stacked on a given disk or partition name.
        let mut dependents: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            for s in &e.slaves {
                dependents.entry(s.clone()).or_default().push(i);
            }
        }
        let known: HashSet<&str> = entries
            .iter()
            .flat_map(|e| {
                std::iter::once(e.sysname.as_str()).chain(e.parts.iter().map(|p| p.name.as_str()))
            })
            .collect();
        // Top level: anything not stacked on another listed device. A device
        // whose only slaves are virtual (dm over loop) still lands here.
        inv.tree = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.slaves.iter().any(|s| known.contains(s.as_str())))
            .map(|(i, _)| build(&entries, &dependents, i, 0))
            .collect();
        inv
    }

    fn build(
        entries: &[Entry],
        dependents: &HashMap<String, Vec<usize>>,
        i: usize,
        depth: usize,
    ) -> Device {
        let e = &entries[i];
        let mut d = e.dev.clone();
        if depth > 16 {
            return d;
        }
        for p in &e.parts {
            let mut p = p.clone();
            p.children = stacked(entries, dependents, &p.name, depth + 1);
            d.children.push(p);
        }
        d.children
            .extend(stacked(entries, dependents, &e.sysname, depth + 1));
        d
    }

    fn stacked(
        entries: &[Entry],
        dependents: &HashMap<String, Vec<usize>>,
        on: &str,
        depth: usize,
    ) -> Vec<Device> {
        dependents
            .get(on)
            .map(|idx| {
                idx.iter()
                    .map(|&i| build(entries, dependents, i, depth))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn swaps() -> Vec<Swap> {
        let Ok(text) = fs::read_to_string("/proc/swaps") else {
            return Vec::new();
        };
        text.lines()
            .skip(1)
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let path = f.next()?;
                let _kind = f.next()?;
                let size: u64 = f.next()?.parse().ok()?;
                let used: u64 = f.next()?.parse().ok()?;
                let dev = super::blkdev_of_path(path)?;
                Some(Swap {
                    dev,
                    size: size * 1024,
                    used: used * 1024,
                })
            })
            .collect()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Inventory;
    use std::process::Command;

    fn diskutil(args: &[&str]) -> Option<Vec<u8>> {
        let out = Command::new("diskutil")
            .arg("list")
            .arg("-plist")
            .args(args)
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    }

    pub fn inventory() -> Inventory {
        match diskutil(&[]) {
            Some(all) => super::diskutil::parse(&all, diskutil(&["physical"]).as_deref()),
            None => Inventory::default(),
        }
    }
}

/// Parser for `diskutil list -plist` output. Platform-independent so it can
/// be tested anywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod diskutil {
    use super::{Device, Inventory};
    use plist::{Dictionary, Value};

    fn get_str<'a>(d: &'a Dictionary, key: &str) -> Option<&'a str> {
        d.get(key)?.as_string()
    }

    fn get_u64(d: &Dictionary, key: &str) -> Option<u64> {
        let v = d.get(key)?;
        v.as_unsigned_integer()
            .or_else(|| v.as_signed_integer().and_then(|i| u64::try_from(i).ok()))
    }

    fn device(id: &str, size: Option<u64>) -> Device {
        Device {
            name: id.to_string(),
            path: format!("/dev/{id}"),
            size,
            dev: None,
            children: Vec::new(),
        }
    }

    fn find_mut<'a>(nodes: &'a mut [Device], name: &str) -> Option<&'a mut Device> {
        for n in nodes.iter_mut() {
            if n.name == name {
                return Some(n);
            }
            if let Some(found) = find_mut(&mut n.children, name) {
                return Some(found);
            }
        }
        None
    }

    fn mark_virtual(inv: &mut Inventory, d: &Device) {
        inv.virtual_paths.push(d.path.clone());
        for c in &d.children {
            mark_virtual(inv, c);
        }
    }

    struct Raw {
        disk: Device,
        /// Partitions backing an APFS container (`APFSPhysicalStores`).
        stores: Vec<String>,
        physical: bool,
    }

    /// `all` is `diskutil list -plist`; `physical` is `diskutil list -plist physical`,
    /// whose `WholeDisks` names the real hardware. Without it, every disk that
    /// is not an APFS container is assumed physical.
    pub fn parse(all: &[u8], physical: Option<&[u8]>) -> Inventory {
        let mut inv = Inventory::default();
        let Ok(root) = Value::from_reader_xml(all) else {
            return inv;
        };
        let Some(disks) = root
            .as_dictionary()
            .and_then(|d| d.get("AllDisksAndPartitions"))
            .and_then(Value::as_array)
        else {
            return inv;
        };
        let physical: Option<Vec<String>> = physical
            .and_then(|p| Value::from_reader_xml(p).ok())
            .and_then(|v| {
                let list = v.as_dictionary()?.get("WholeDisks")?.as_array()?;
                Some(
                    list.iter()
                        .filter_map(|x| x.as_string().map(str::to_string))
                        .collect(),
                )
            });

        let mut raws: Vec<Raw> = Vec::new();
        for d in disks.iter().filter_map(Value::as_dictionary) {
            let Some(id) = get_str(d, "DeviceIdentifier") else {
                continue;
            };
            let mut disk = device(id, get_u64(d, "Size"));
            for key in ["Partitions", "APFSVolumes"] {
                let Some(arr) = d.get(key).and_then(Value::as_array) else {
                    continue;
                };
                for p in arr.iter().filter_map(Value::as_dictionary) {
                    if let Some(pid) = get_str(p, "DeviceIdentifier") {
                        disk.children.push(device(pid, get_u64(p, "Size")));
                    }
                }
            }
            let stores: Vec<String> = d
                .get("APFSPhysicalStores")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| {
                            s.as_dictionary()
                                .and_then(|s| get_str(s, "DeviceIdentifier"))
                        })
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let is_physical = match &physical {
                Some(list) => list.iter().any(|p| p == id),
                None => stores.is_empty(),
            };
            raws.push(Raw {
                disk,
                stores,
                physical: is_physical,
            });
        }

        let mut tree: Vec<Device> = raws
            .iter()
            .filter(|r| r.physical)
            .map(|r| r.disk.clone())
            .collect();
        let mut leftover: Vec<Raw> = raws.into_iter().filter(|r| !r.physical).collect();
        // Hang APFS containers off their physical store partition. Repeat
        // until nothing more attaches (a container can sit on a container).
        loop {
            let mut progress = false;
            leftover.retain(|r| {
                for store in &r.stores {
                    if let Some(node) = find_mut(&mut tree, store) {
                        node.children.push(r.disk.clone());
                        progress = true;
                        return false;
                    }
                }
                true
            });
            if !progress {
                break;
            }
        }
        // Whatever is left is a disk image or similar: not real hardware.
        for r in &leftover {
            mark_virtual(&mut inv, &r.disk);
        }
        inv.tree = tree;
        inv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["sda10", "sdb", "sda2", "sdaa", "nvme0n1", "sda"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["nvme0n1", "sda", "sda2", "sda10", "sdaa", "sdb"]);
    }

    #[test]
    fn devnum_parsing() {
        assert_eq!(parse_devnum("8:1"), Some((8, 1)));
        assert_eq!(parse_devnum(" 259:3\n"), Some((259, 3)));
        assert_eq!(parse_devnum("nope"), None);
        assert_eq!(devnum_from_rdev(0x801), (8, 1));
        assert_eq!(devnum_from_rdev(0x10300), (259, 0)); // nvme0n1
        assert_eq!(devnum_from_rdev(0x10002), (256, 2)); // 12-bit major
        assert_eq!(devnum_from_rdev(0x100802), (8, 258)); // minor above 255 lives in bits 20+
    }

    fn plist_doc(body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>{body}</dict></plist>"
        )
    }

    const ALL: &str = r#"
<key>AllDisksAndPartitions</key>
<array>
  <dict>
    <key>Content</key><string>GUID_partition_scheme</string>
    <key>DeviceIdentifier</key><string>disk0</string>
    <key>OSInternal</key><false/>
    <key>Partitions</key>
    <array>
      <dict>
        <key>Content</key><string>Apple_APFS_ISC</string>
        <key>DeviceIdentifier</key><string>disk0s1</string>
        <key>Size</key><integer>524288000</integer>
      </dict>
      <dict>
        <key>Content</key><string>Apple_APFS</string>
        <key>DeviceIdentifier</key><string>disk0s2</string>
        <key>Size</key><integer>494384795648</integer>
      </dict>
    </array>
    <key>Size</key><integer>500277790720</integer>
  </dict>
  <dict>
    <key>APFSPhysicalStores</key>
    <array><dict><key>DeviceIdentifier</key><string>disk0s2</string></dict></array>
    <key>APFSVolumes</key>
    <array>
      <dict>
        <key>DeviceIdentifier</key><string>disk3s1</string>
        <key>MountedSnapshots</key>
        <array><dict><key>SnapshotMountPoint</key><string>/</string></dict></array>
        <key>Size</key><integer>494384795648</integer>
        <key>VolumeName</key><string>Macintosh HD</string>
      </dict>
      <dict>
        <key>DeviceIdentifier</key><string>disk3s5</string>
        <key>MountPoint</key><string>/System/Volumes/Data</string>
        <key>Size</key><integer>494384795648</integer>
        <key>VolumeName</key><string>Data</string>
      </dict>
    </array>
    <key>Content</key><string>Apple_APFS_Container</string>
    <key>DeviceIdentifier</key><string>disk3</string>
    <key>Size</key><integer>494384795648</integer>
  </dict>
  <dict>
    <key>Content</key><string>GUID_partition_scheme</string>
    <key>DeviceIdentifier</key><string>disk5</string>
    <key>Partitions</key>
    <array>
      <dict>
        <key>Content</key><string>Apple_HFS</string>
        <key>DeviceIdentifier</key><string>disk5s1</string>
        <key>MountPoint</key><string>/Volumes/Installer</string>
        <key>Size</key><integer>104857600</integer>
      </dict>
    </array>
    <key>Size</key><integer>104857600</integer>
  </dict>
</array>
<key>WholeDisks</key>
<array><string>disk0</string><string>disk3</string><string>disk5</string></array>
"#;

    const PHYSICAL: &str = r#"
<key>AllDisksAndPartitions</key><array/>
<key>WholeDisks</key><array><string>disk0</string></array>
"#;

    #[test]
    fn diskutil_tree_with_physical_listing() {
        let inv = diskutil::parse(
            plist_doc(ALL).as_bytes(),
            Some(plist_doc(PHYSICAL).as_bytes()),
        );
        assert_eq!(inv.tree.len(), 1, "only disk0 is physical");
        let disk0 = &inv.tree[0];
        assert_eq!(disk0.name, "disk0");
        assert_eq!(disk0.path, "/dev/disk0");
        assert_eq!(disk0.size, Some(500277790720));
        assert_eq!(disk0.children.len(), 2);
        let disk0s2 = &disk0.children[1];
        assert_eq!(disk0s2.name, "disk0s2");
        assert_eq!(
            disk0s2.children.len(),
            1,
            "APFS container attaches to its physical store"
        );
        let container = &disk0s2.children[0];
        assert_eq!(container.name, "disk3");
        let vols: Vec<&str> = container.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(vols, ["disk3s1", "disk3s5"]);
        // The disk image is not hardware.
        assert_eq!(inv.virtual_paths, ["/dev/disk5", "/dev/disk5s1"]);
        assert!(inv.is_virtual_source("/dev/disk5s1", None));
        assert!(!inv.is_virtual_source("/dev/disk3s1", None));
    }

    #[test]
    fn diskutil_tree_without_physical_listing() {
        let inv = diskutil::parse(plist_doc(ALL).as_bytes(), None);
        let names: Vec<&str> = inv.tree.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["disk0", "disk5"],
            "without the physical list, non-containers count as disks"
        );
        assert!(inv.virtual_paths.is_empty());
    }

    /// Build a fake /sys/block: a disk with two partitions, an LVM volume
    /// on the second partition, and a loop device (virtual).
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sysfs_tree() {
        let root = std::env::temp_dir().join(format!("diskdingo-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str, files: &[(&str, &str)]| {
            let d = root.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            for (name, content) in files {
                std::fs::write(d.join(name), content).unwrap();
            }
        };
        mk("sda", &[("size", "2000\n"), ("dev", "8:0\n")]);
        mk(
            "sda/sda1",
            &[("size", "100\n"), ("dev", "8:1\n"), ("partition", "1\n")],
        );
        mk(
            "sda/sda2",
            &[("size", "1900\n"), ("dev", "8:2\n"), ("partition", "2\n")],
        );
        mk("sda/slaves", &[]);
        mk("dm-0", &[("size", "1800\n"), ("dev", "254:0\n")]);
        mk("dm-0/dm", &[("name", "vg-root\n")]);
        mk("dm-0/slaves", &[]);
        std::os::unix::fs::symlink("../../sda/sda2", root.join("dm-0/slaves/sda2")).unwrap();
        mk("loop0", &[("size", "50\n"), ("dev", "7:0\n")]);

        let inv = linux::inventory_at(&root);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            inv.tree.len(),
            1,
            "dm-0 hangs off sda2, loop0 is virtual: {:?}",
            inv.tree
        );
        let sda = &inv.tree[0];
        assert_eq!(
            (sda.name.as_str(), sda.size, sda.dev),
            ("sda", Some(2000 * 512), Some((8, 0)))
        );
        let parts: Vec<&str> = sda.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(parts, ["sda1", "sda2"]);
        let lv = &sda.children[1].children[0];
        assert_eq!(
            (lv.name.as_str(), lv.path.as_str(), lv.dev),
            ("vg-root", "/dev/mapper/vg-root", Some((254, 0)))
        );
        assert_eq!(inv.virtual_devs, [(7, 0)]);
        assert_eq!(inv.virtual_paths, ["/dev/loop0"]);
    }

    #[test]
    fn diskutil_garbage_is_empty() {
        assert_eq!(diskutil::parse(b"not a plist", None), Inventory::default());
    }
}
