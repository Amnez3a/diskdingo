//! Enumerate mounted filesystems.
//!
//! Linux reads `/proc/self/mountinfo`; macOS asks the kernel with
//! `getfsstat(2)`. Both produce the same [`Mount`] record.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// What was mounted: a device path, `server:/export`, `//host/share`, ...
    pub source: String,
    /// Where it is mounted.
    pub target: String,
    pub fstype: String,
    /// Subpath of the source filesystem that is mounted. Anything other
    /// than "/" means a bind mount or a subvolume (Linux only; always "/"
    /// on macOS).
    pub root: String,
    /// major:minor of the backing block device, when there is one.
    pub blkdev: Option<(u32, u32)>,
}

#[cfg(target_os = "linux")]
pub fn list_mounts() -> std::io::Result<Vec<Mount>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mut mounts = parse_mountinfo(&text);
    // Filesystems such as btrfs and fuseblk report an anonymous device
    // (major 0) in mountinfo; find the real block device through the
    // source path instead.
    for m in &mut mounts {
        if m.blkdev.is_none() && m.source.starts_with("/dev/") {
            m.blkdev = crate::devices::blkdev_of_path(&m.source);
        }
    }
    Ok(mounts)
}

/// Parse the contents of `/proc/self/mountinfo`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines().filter_map(parse_mountinfo_line).collect()
}

fn parse_mountinfo_line(line: &str) -> Option<Mount> {
    // 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
    // (1)(2)(3)   (4)   (5)      (6)      (7)   (8) (9)   (10)         (11)
    let mut f = line.split(' ');
    let _mount_id = f.next()?;
    let _parent_id = f.next()?;
    let devnums = f.next()?;
    let root = unescape(f.next()?);
    let target = unescape(f.next()?);
    let _options = f.next()?;
    let mut f = f.skip_while(|x| *x != "-");
    f.next()?; // the "-" separator
    let fstype = f.next()?.to_string();
    let source = unescape(f.next()?);
    let blkdev = crate::devices::parse_devnum(devnums).filter(|(major, _)| *major != 0);
    Some(Mount {
        source,
        target,
        fstype,
        root,
        blkdev,
    })
}

/// Undo the octal escapes mountinfo uses for space, tab, newline and backslash.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match (digits.len() == 3)
            .then(|| u8::from_str_radix(&digits, 8).ok())
            .flatten()
        {
            Some(b) => {
                out.push(b as char);
                chars.nth(2);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(target_os = "macos")]
pub fn list_mounts() -> std::io::Result<Vec<Mount>> {
    // MNT_NOWAIT: report cached data rather than asking every (possibly
    // hung) network server for fresh statistics.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(n as usize + 16);
    let bytes = (buf.capacity() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel filled `n` entries, and n <= capacity.
    unsafe { buf.set_len(n as usize) };
    Ok(buf
        .iter()
        .map(|st| Mount {
            source: cstr(&st.f_mntfromname),
            target: cstr(&st.f_mntonname),
            fstype: cstr(&st.f_fstypename),
            root: "/".to_string(),
            blkdev: None,
        })
        .collect())
}

#[cfg(target_os = "macos")]
fn cstr(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
29 1 0:27 /@ / rw,relatime shared:1 - btrfs /dev/sda2 rw,compress=zstd:3,subvol=/@
243 29 8:1 / /boot rw,relatime shared:215 - vfat /dev/sda1 rw,fmask=0022
56 29 0:27 /@home /home rw,relatime shared:191 - btrfs /dev/sda2 rw,subvol=/@home
135 58 0:76 / /better-otter2 rw,relatime shared:490 - cifs //better-otter2.lab.net/tank1 rw,vers=3.1.1
203 29 0:62 / /tmp rw,nosuid,nodev shared:203 - tmpfs tmpfs rw,inode64
824 27 8:49 / /run/media/user1/My\\040Disk ro,nosuid shared:749 - vfat /dev/sdd1 ro
garbage line";

    #[test]
    fn parses_mountinfo_fields() {
        let m = parse_mountinfo(SAMPLE);
        assert_eq!(m.len(), 6);
        assert_eq!(m[0].source, "/dev/sda2");
        assert_eq!(m[0].target, "/");
        assert_eq!(m[0].fstype, "btrfs");
        assert_eq!(m[0].root, "/@");
        assert_eq!(
            m[0].blkdev, None,
            "anonymous major 0 must not count as a block device"
        );
        assert_eq!(m[1].blkdev, Some((8, 1)));
        assert_eq!(m[1].root, "/");
        assert_eq!(m[2].root, "/@home");
        assert_eq!(m[3].source, "//better-otter2.lab.net/tank1");
        assert_eq!(m[3].fstype, "cifs");
        assert_eq!(m[4].source, "tmpfs");
        assert_eq!(m[5].target, "/run/media/user1/My Disk");
        assert_eq!(m[5].blkdev, Some((8, 49)));
    }

    #[test]
    fn unescapes_octal() {
        assert_eq!(unescape("a\\040b\\011c\\134d"), "a b\tc\\d");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("trail\\04"), "trail\\04");
        assert_eq!(unescape("bad\\zzz"), "bad\\zzz");
    }
}
