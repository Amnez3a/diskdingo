//! Block device inventory: physical disks, their partitions, and whatever
//! is stacked on top of them (LVM, md, APFS containers and volumes).
//!
//! Linux walks `/sys/block`; macOS parses `diskutil list -plist`; Windows
//! walks volumes and physical drives through Win32.

use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    /// Display name: `sda1`, `vg-root`, `disk0s2`, `PhysicalDrive0`.
    pub name: String,
    /// Device node, used to match mount sources: `/dev/sda1`,
    /// `/dev/mapper/vg-root`, `\\?\Volume{...}\`.
    pub path: String,
    /// Size in bytes, when known.
    pub size: Option<u64>,
    /// major:minor (Linux only).
    pub dev: Option<(u32, u32)>,
    /// Persistent identifier for the `-u` column: the `/dev/disk/by-id`
    /// path (Linux), the media UUID (macOS), the volume GUID path (Windows).
    pub id: Option<String>,
    pub children: Vec<Device>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Top-level (physical) disks with their partitions nested below.
    pub tree: Vec<Device>,
    /// Block devices that are not real disks: loop devices, zram, mounted
    /// disk images. Filesystems on them are treated as pseudo mounts.
    pub virtual_devs: Vec<(u32, u32)>,
    pub virtual_paths: Vec<String>,
    /// Filesystem UUID by block device (Linux `/dev/disk/by-uuid`).
    pub fs_uuids: HashMap<(u32, u32), String>,
}

impl Inventory {
    pub fn is_virtual_source(&self, source: &str, blkdev: Option<(u32, u32)>) -> bool {
        blkdev.is_some_and(|d| self.virtual_devs.contains(&d))
            || self.virtual_paths.iter().any(|p| p == source)
    }

    /// Identifier for a mount row: the filesystem UUID when the backing
    /// device is known, or the Windows volume GUID path.
    pub fn mount_id(&self, m: &crate::mounts::Mount) -> Option<String> {
        if let Some(uuid) = m.blkdev.and_then(|d| self.fs_uuids.get(&d)) {
            return Some(uuid.clone());
        }
        m.source
            .starts_with("\\\\?\\Volume{")
            .then(|| m.source.clone())
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
    #[cfg(windows)]
    {
        windows::scan().inventory.clone()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn take_number(it: &mut std::iter::Peekable<std::str::Chars>) -> u64 {
    let mut n = 0u64;
    while let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
        n = n.saturating_mul(10).saturating_add(u64::from(d));
        it.next();
    }
    n
}

/// Pick the `/dev/disk/by-id` name to hand to `zpool create`.
///
/// A WWN / EUI-64 name identifies the drive itself, independent of the
/// controller or enclosure it is plugged into, so it wins. Next come
/// interface+model+serial names (`ata-`, `nvme-`, `usb-`, ...) and the
/// UUID-based names of md/LVM devices. Anything else (`dm-name-`,
/// `google-`, ...) is a last resort. Ties go to the shortest name, which
/// drops the `_1` namespace duplicates newer udev adds for NVMe.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn best_id<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    fn tier(name: &str) -> u8 {
        const STABLE: [&str; 3] = ["wwn-", "nvme-eui.", "scsi-3"];
        const SERIAL: [&str; 13] = [
            "ata-",
            "nvme-",
            "scsi-S",
            "scsi-1",
            "scsi-2",
            "usb-",
            "mmc-",
            "virtio-",
            "ieee1394-",
            "md-uuid-",
            "dm-uuid-",
            "lvm-pv-uuid-",
            "sas-",
        ];
        if STABLE.iter().any(|p| name.starts_with(p)) {
            0
        } else if SERIAL.iter().any(|p| name.starts_with(p)) {
            1
        } else {
            2
        }
    }
    names.into_iter().min_by_key(|n| (tier(n), n.len(), *n))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{best_id, natural_cmp, parse_devnum, Device, Inventory, Swap};
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

    /// Symlink names in a `/dev/disk/by-*` directory, grouped by the block
    /// device they point to.
    fn links_by_devnum(dir: &Path) -> HashMap<(u32, u32), Vec<String>> {
        let mut out: HashMap<(u32, u32), Vec<String>> = HashMap::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return out;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(dev) = super::blkdev_of_path(&e.path().to_string_lossy()) {
                out.entry(dev).or_default().push(name);
            }
        }
        out
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
                    id: None,
                    children: Vec::new(),
                },
            ));
        }
        parts.sort_by_key(|(n, _)| *n);
        parts.into_iter().map(|(_, d)| d).collect()
    }

    pub fn inventory() -> Inventory {
        inventory_at(Path::new("/sys/block"), Path::new("/dev/disk"))
    }

    /// `sys_block` is `/sys/block`; `dev_disk` is `/dev/disk` (for the
    /// `by-id` and `by-uuid` links).
    pub fn inventory_at(sys_block: &Path, dev_disk: &Path) -> Inventory {
        let mut inv = Inventory::default();
        let Ok(rd) = fs::read_dir(sys_block) else {
            return inv;
        };
        let ids = links_by_devnum(&dev_disk.join("by-id"));
        let id_for = |dev: Option<(u32, u32)>| -> Option<String> {
            let names = ids.get(&dev?)?;
            best_id(names.iter().map(String::as_str)).map(|n| format!("/dev/disk/by-id/{n}"))
        };
        inv.fs_uuids = links_by_devnum(&dev_disk.join("by-uuid"))
            .into_iter()
            .filter_map(|(dev, mut names)| {
                names.sort();
                Some((dev, names.into_iter().next()?))
            })
            .collect();

        let mut entries: Vec<Entry> = Vec::new();
        for e in rd.flatten() {
            let sysname = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            let dev = read_dev(&dir);
            let mut parts = partitions(&dir, &sysname);
            if is_virtual(&sysname) {
                inv.virtual_devs
                    .extend(dev.into_iter().chain(parts.iter().filter_map(|p| p.dev)));
                inv.virtual_paths.push(format!("/dev/{sysname}"));
                inv.virtual_paths.extend(parts.into_iter().map(|p| p.path));
                continue;
            }
            for p in &mut parts {
                p.id = id_for(p.dev);
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
                    id: id_for(dev),
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

    fn device(d: &Dictionary, id: &str) -> Device {
        Device {
            name: id.to_string(),
            path: format!("/dev/{id}"),
            size: get_u64(d, "Size"),
            dev: None,
            // The media UUID: what OpenZFS on OS X keys /var/run/disk/by-id on.
            id: get_str(d, "DiskUUID")
                .or_else(|| get_str(d, "VolumeUUID"))
                .map(str::to_string),
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
            let mut disk = device(d, id);
            for key in ["Partitions", "APFSVolumes"] {
                let Some(arr) = d.get(key).and_then(Value::as_array) else {
                    continue;
                };
                for p in arr.iter().filter_map(Value::as_dictionary) {
                    if let Some(pid) = get_str(p, "DeviceIdentifier") {
                        disk.children.push(device(p, pid));
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

/// Windows: volumes come from `FindFirstVolume`, physical drives from
/// `\\.\PhysicalDriveN`, and the two are tied together with
/// `IOCTL_STORAGE_GET_DEVICE_NUMBER`. Everything is opened with zero
/// access rights, which needs no administrator privileges.
#[cfg(windows)]
pub mod windows {
    use super::{Device, Inventory};
    use crate::mounts::{Class, Mount};
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::mem::{offset_of, size_of};
    use std::ptr::{null, null_mut, read_unaligned};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::{
        BusTypeFileBackedVirtual, CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNamesForVolumeNameW,
        QueryDosDeviceW, FILE_DEVICE_CD_ROM, FILE_DEVICE_DVD, FILE_SHARE_READ, FILE_SHARE_WRITE,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, DISK_EXTENT, DISK_GEOMETRY_EX,
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_GET_DEVICE_NUMBER,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
        STORAGE_PROPERTY_QUERY, VOLUME_DISK_EXTENTS,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_RAMDISK, DRIVE_REMOTE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    pub struct Scan {
        pub mounts: Vec<Mount>,
        pub inventory: Inventory,
    }

    /// Mounts and devices are found in one pass; both callers share it.
    pub fn scan() -> &'static Scan {
        static SCAN: OnceLock<Scan> = OnceLock::new();
        SCAN.get_or_init(build)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Open a device with zero access rights: enough for the query IOCTLs
    /// used here, and allowed for ordinary users.
    fn open(path: &str) -> Option<Handle> {
        let p = wide(path);
        let h = unsafe {
            CreateFileW(
                p.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        (h != INVALID_HANDLE_VALUE).then_some(Handle(h))
    }

    fn ioctl(h: &Handle, code: u32, input: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut n = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                h.0,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut n,
                null_mut(),
            )
        };
        (ok != 0).then_some(n as usize)
    }

    fn device_number(h: &Handle) -> Option<STORAGE_DEVICE_NUMBER> {
        let mut out = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
        ioctl(h, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out)?;
        Some(unsafe { read_unaligned(out.as_ptr().cast()) })
    }

    fn extents(h: &Handle) -> Vec<DISK_EXTENT> {
        const MAX: usize = 64;
        let mut out =
            vec![0u8; offset_of!(VOLUME_DISK_EXTENTS, Extents) + MAX * size_of::<DISK_EXTENT>()];
        if ioctl(h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut out).is_none() {
            return Vec::new();
        }
        let n = u32::from_ne_bytes([out[0], out[1], out[2], out[3]]) as usize;
        (0..n.min(MAX))
            .map(|i| {
                let at = offset_of!(VOLUME_DISK_EXTENTS, Extents) + i * size_of::<DISK_EXTENT>();
                unsafe { read_unaligned(out.as_ptr().add(at).cast()) }
            })
            .collect()
    }

    fn disk_size(h: &Handle) -> Option<u64> {
        let mut out = [0u8; 512];
        ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out)?;
        let size: i64 = unsafe {
            read_unaligned(
                out.as_ptr()
                    .add(offset_of!(DISK_GEOMETRY_EX, DiskSize))
                    .cast(),
            )
        };
        u64::try_from(size).ok()
    }

    /// VHD/VHDX and other file-backed disks are disk images, not hardware.
    fn is_file_backed(h: &Handle) -> bool {
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let input = unsafe {
            std::slice::from_raw_parts(
                (&query as *const STORAGE_PROPERTY_QUERY).cast::<u8>(),
                size_of::<STORAGE_PROPERTY_QUERY>(),
            )
        };
        let mut out = [0u8; 1024];
        if ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, input, &mut out).is_none() {
            return false;
        }
        let bus: i32 = unsafe {
            read_unaligned(
                out.as_ptr()
                    .add(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType))
                    .cast(),
            )
        };
        bus == BusTypeFileBackedVirtual
    }

    /// Every volume GUID path (`\\?\Volume{...}\`).
    fn volumes() -> Vec<String> {
        let mut buf = [0u16; 128];
        let h = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
        if h == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut out = vec![from_wide(&buf)];
        while unsafe { FindNextVolumeW(h, buf.as_mut_ptr(), buf.len() as u32) } != 0 {
            out.push(from_wide(&buf));
        }
        unsafe { FindVolumeClose(h) };
        out
    }

    /// Drive letters and folders where a volume is mounted.
    fn path_names(volume: &str) -> Vec<String> {
        let v = wide(volume);
        let mut buf = vec![0u16; 8192];
        let mut len = 0u32;
        if unsafe {
            GetVolumePathNamesForVolumeNameW(
                v.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut len,
            )
        } == 0
        {
            return Vec::new();
        }
        buf[..(len as usize).min(buf.len())]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }

    fn drive_type(root: &str) -> u32 {
        let r = wide(root);
        unsafe { GetDriveTypeW(r.as_ptr()) }
    }

    fn fs_name(root: &str) -> Option<String> {
        let r = wide(root);
        let mut fs = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                r.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        (ok != 0).then(|| from_wide(&fs)).filter(|s| !s.is_empty())
    }

    /// `\\server\share` behind a mapped drive letter (`Z:`).
    fn remote_name(local: &str) -> Option<String> {
        let l = wide(local);
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        (unsafe { WNetGetConnectionW(l.as_ptr(), buf.as_mut_ptr(), &mut len) } == NO_ERROR)
            .then(|| from_wide(&buf))
    }

    /// What a drive letter points at: `\??\C:\dir` for `subst`, or
    /// `\Device\HarddiskVolume3` for a real volume.
    fn dos_target(local: &str) -> Option<String> {
        let l = wide(local);
        let mut buf = vec![0u16; 1024];
        (unsafe { QueryDosDeviceW(l.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } != 0)
            .then(|| from_wide(&buf))
    }

    struct Vol {
        guid: String,
        kind: u32,
        fstype: Option<String>,
        paths: Vec<String>,
        number: Option<STORAGE_DEVICE_NUMBER>,
        disk: Option<u32>,
        size: Option<u64>,
    }

    fn build() -> Scan {
        let mut mounts: Vec<Mount> = Vec::new();
        let mut inv = Inventory::default();

        let mut vols: Vec<Vol> = volumes()
            .into_iter()
            .map(|guid| {
                let handle = open(guid.trim_end_matches('\\'));
                let number = handle.as_ref().and_then(device_number);
                let ext = handle.as_ref().map(extents).unwrap_or_default();
                let disk = number
                    .map(|n| n.DeviceNumber)
                    .filter(|&n| n != u32::MAX)
                    .or_else(|| ext.first().map(|e| e.DiskNumber));
                let size = (!ext.is_empty())
                    .then(|| ext.iter().map(|e| e.ExtentLength.max(0) as u64).sum());
                Vol {
                    kind: drive_type(&guid),
                    fstype: fs_name(&guid),
                    paths: path_names(&guid),
                    number,
                    disk,
                    size,
                    guid,
                }
            })
            .collect();
        vols.sort_by_key(|v| (v.disk, v.number.map(|n| n.PartitionNumber), v.guid.clone()));

        // Physical drives: probe the first 64 numbers, and anything a volume
        // referred to beyond that.
        let mut disks: BTreeMap<u32, Device> = BTreeMap::new();
        let mut virtual_disks: BTreeSet<u32> = BTreeSet::new();
        let referenced: BTreeSet<u32> = vols.iter().filter_map(|v| v.disk).collect();
        for n in (0..64).chain(referenced.iter().copied()) {
            if disks.contains_key(&n) {
                continue;
            }
            let path = format!("\\\\.\\PhysicalDrive{n}");
            let Some(h) = open(&path) else { continue };
            if is_file_backed(&h) {
                virtual_disks.insert(n);
            }
            disks.insert(
                n,
                Device {
                    name: format!("PhysicalDrive{n}"),
                    path,
                    size: disk_size(&h),
                    ..Device::default()
                },
            );
        }
        for n in &referenced {
            disks.entry(*n).or_insert_with(|| Device {
                name: format!("PhysicalDrive{n}"),
                path: format!("\\\\.\\PhysicalDrive{n}"),
                ..Device::default()
            });
        }

        let mut cdroms: Vec<Device> = Vec::new();
        for v in vols {
            let is_cdrom = v.kind == DRIVE_CDROM
                || v.number.is_some_and(|n| {
                    n.DeviceType == FILE_DEVICE_CD_ROM || n.DeviceType == FILE_DEVICE_DVD
                });
            let is_virtual =
                v.kind == DRIVE_RAMDISK || v.disk.is_some_and(|d| virtual_disks.contains(&d));
            let label = match (v.disk, v.number) {
                (Some(d), _) if is_cdrom => format!("CdRom{d}"),
                (Some(d), Some(n)) => format!("Harddisk{d}Partition{}", n.PartitionNumber),
                _ => v.guid.clone(),
            };
            let node = Device {
                name: match (v.disk, v.number) {
                    (Some(d), _) if is_cdrom => format!("CdRom{d}"),
                    (_, Some(n)) if !is_cdrom => format!("Partition{}", n.PartitionNumber),
                    _ => v.guid.clone(),
                },
                path: v.guid.clone(),
                size: v.size,
                dev: None,
                id: Some(v.guid.clone()),
                children: Vec::new(),
            };
            if is_virtual {
                inv.virtual_paths.push(v.guid.clone());
            } else if is_cdrom {
                cdroms.push(node);
            } else if let Some(disk) = v.disk.and_then(|d| disks.get_mut(&d)) {
                disk.children.push(node);
            }
            // A volume with no drive letter or folder is still reachable
            // through its GUID path; a drive with no media has no filesystem.
            let Some(fstype) = v.fstype else { continue };
            let targets = if v.paths.is_empty() {
                vec![v.guid.clone()]
            } else {
                v.paths.clone()
            };
            for target in targets {
                mounts.push(Mount {
                    label: Some(label.clone()),
                    class: Some(if is_virtual {
                        Class::Pseudo
                    } else {
                        Class::Block
                    }),
                    ..Mount::new(&v.guid, &target, &fstype)
                });
            }
        }
        inv.tree = disks
            .into_iter()
            .filter(|(n, _)| !virtual_disks.contains(n))
            .map(|(_, d)| d)
            .chain(cdroms)
            .collect();

        // Drive letters that are not volumes: network drives and `subst`.
        let covered: HashSet<String> = mounts.iter().map(|m| m.target.to_uppercase()).collect();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            if covered.contains(&root) {
                continue;
            }
            let local = format!("{letter}:");
            if drive_type(&root) == DRIVE_REMOTE {
                let source = remote_name(&local).unwrap_or_else(|| root.clone());
                mounts.push(Mount {
                    class: Some(Class::Network),
                    ..Mount::new(&source, &root, "remote")
                });
                continue;
            }
            let Some(target) = dos_target(&local) else {
                continue;
            };
            let source = target.strip_prefix("\\??\\").unwrap_or(&target).to_string();
            let class = if source.starts_with("\\Device\\") {
                Class::Block
            } else {
                Class::Other
            };
            let fstype = fs_name(&root).unwrap_or_else(|| "-".to_string());
            mounts.push(Mount {
                class: Some(class),
                ..Mount::new(&source, &root, &fstype)
            });
        }
        Scan {
            mounts,
            inventory: inv,
        }
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

    #[test]
    fn picks_the_most_stable_by_id_name() {
        let sata = [
            "ata-WDC_WD40EFRX-68N32N0_WD-WCC7K1234567",
            "wwn-0x50014ee2b6e1a5b3",
        ];
        assert_eq!(best_id(sata), Some("wwn-0x50014ee2b6e1a5b3"));
        let usb = ["usb-Seagate_Expansion_NA1234-0:0"];
        assert_eq!(best_id(usb), Some("usb-Seagate_Expansion_NA1234-0:0"));
        let nvme = [
            "nvme-Samsung_SSD_980_1TB_S5GXNX0R123456A_1",
            "nvme-eui.0025385b21b0c123",
            "nvme-Samsung_SSD_980_1TB_S5GXNX0R123456A",
        ];
        assert_eq!(best_id(nvme), Some("nvme-eui.0025385b21b0c123"));
        let nvme_no_eui = [
            "nvme-Samsung_SSD_980_1TB_S5GXNX0R123456A_1",
            "nvme-Samsung_SSD_980_1TB_S5GXNX0R123456A",
        ];
        assert_eq!(
            best_id(nvme_no_eui),
            Some("nvme-Samsung_SSD_980_1TB_S5GXNX0R123456A")
        );
        let lvm = ["dm-name-vg-root", "dm-uuid-LVM-abc123"];
        assert_eq!(best_id(lvm), Some("dm-uuid-LVM-abc123"));
        let part = [
            "ata-WDC_WD40EFRX-68N32N0_WD-WCC7K1234567-part1",
            "wwn-0x50014ee2b6e1a5b3-part1",
        ];
        assert_eq!(best_id(part), Some("wwn-0x50014ee2b6e1a5b3-part1"));
        assert_eq!(best_id([]), None);
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
        <key>DiskUUID</key><string>7F1A2B3C-0000-4000-8000-000000000001</string>
        <key>Size</key><integer>524288000</integer>
      </dict>
      <dict>
        <key>Content</key><string>Apple_APFS</string>
        <key>DeviceIdentifier</key><string>disk0s2</string>
        <key>DiskUUID</key><string>7F1A2B3C-0000-4000-8000-000000000002</string>
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
        <key>DiskUUID</key><string>7F1A2B3C-0000-4000-8000-000000000031</string>
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
        assert_eq!(disk0.id, None);
        assert_eq!(disk0.children.len(), 2);
        let disk0s2 = &disk0.children[1];
        assert_eq!(disk0s2.name, "disk0s2");
        assert_eq!(
            disk0s2.id.as_deref(),
            Some("7F1A2B3C-0000-4000-8000-000000000002")
        );
        assert_eq!(
            disk0s2.children.len(),
            1,
            "APFS container attaches to its physical store"
        );
        let container = &disk0s2.children[0];
        assert_eq!(container.name, "disk3");
        let vols: Vec<&str> = container.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(vols, ["disk3s1", "disk3s5"]);
        assert_eq!(
            container.children[0].id.as_deref(),
            Some("7F1A2B3C-0000-4000-8000-000000000031")
        );
        assert_eq!(container.children[1].id, None);
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

    /// Build a fake /sys/block and /dev/disk: a disk with two partitions,
    /// an LVM volume on the second partition, a loop device (virtual), and
    /// by-id / by-uuid links for the real devices.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sysfs_tree() {
        let root = std::env::temp_dir().join(format!("diskdingo-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sys = root.join("block");
        let mk = |rel: &str, files: &[(&str, &str)]| {
            let d = sys.join(rel);
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
        std::os::unix::fs::symlink("../../sda/sda2", sys.join("dm-0/slaves/sda2")).unwrap();
        mk("loop0", &[("size", "50\n"), ("dev", "7:0\n")]);

        // by-id links can only be checked against real block devices, so
        // point them at whatever /dev/sda* exists here (if anything).
        let dev_disk = root.join("disk");
        std::fs::create_dir_all(dev_disk.join("by-id")).unwrap();
        std::fs::create_dir_all(dev_disk.join("by-uuid")).unwrap();
        let real_sda = std::path::Path::new("/dev/sda").exists();
        if real_sda {
            std::os::unix::fs::symlink("/dev/sda", dev_disk.join("by-id/ata-FAKE_MODEL_SERIAL"))
                .unwrap();
            std::os::unix::fs::symlink("/dev/sda", dev_disk.join("by-id/wwn-0xfake")).unwrap();
        }

        let inv = linux::inventory_at(&sys, &dev_disk);
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
        if real_sda && blkdev_of_path("/dev/sda") == Some((8, 0)) {
            assert_eq!(sda.id.as_deref(), Some("/dev/disk/by-id/wwn-0xfake"));
        }
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
