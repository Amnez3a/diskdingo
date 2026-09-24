//! Filesystem space statistics, gathered concurrently with a deadline so a
//! hung network mount cannot stall the whole listing.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

/// Space statistics for the filesystem mounted at `path`.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)] // field widths differ between libc targets
pub fn stat_path(path: &str) -> Option<Usage> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(from_blocks(
        st.f_frsize as u64,
        st.f_blocks as u64,
        st.f_bfree as u64,
        st.f_bavail as u64,
    ))
}

/// Space statistics for the filesystem mounted at `path`.
///
/// macOS's `statvfs` still uses 32-bit block counts, which overflow on
/// volumes above 16 TB, so use `statfs` (64-bit) there.
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_cast)] // field widths differ between libc targets
pub fn stat_path(path: &str) -> Option<Usage> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(from_blocks(
        st.f_bsize as u64,
        st.f_blocks as u64,
        st.f_bfree as u64,
        st.f_bavail as u64,
    ))
}

/// Space statistics for the volume at `path` (`C:\`, `\\?\Volume{..}\`, `Z:\`).
#[cfg(windows)]
pub fn stat_path(path: &str) -> Option<Usage> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then(|| Usage {
        size: total,
        used: total.saturating_sub(free),
        avail,
    })
}

#[cfg_attr(windows, allow(dead_code))]
fn from_blocks(bsize: u64, blocks: u64, bfree: u64, bavail: u64) -> Usage {
    Usage {
        size: blocks.saturating_mul(bsize),
        used: blocks.saturating_sub(bfree).saturating_mul(bsize),
        avail: bavail.saturating_mul(bsize),
    }
}

/// Stat every path concurrently. Paths that fail, or that have not
/// answered by the deadline, map to `None`. Threads still blocked in the
/// kernel are abandoned; they die with the process.
pub fn stat_all(paths: &[String], timeout: Duration) -> HashMap<String, Option<Usage>> {
    let mut out: HashMap<String, Option<Usage>> = paths.iter().map(|p| (p.clone(), None)).collect();
    let (tx, rx) = mpsc::channel();
    let mut pending = 0;
    for path in out.keys() {
        let tx = tx.clone();
        let path = path.clone();
        pending += 1;
        std::thread::spawn(move || {
            let usage = stat_path(&path);
            let _ = tx.send((path, usage));
        });
    }
    drop(tx);

    let deadline = Instant::now() + timeout;
    while pending > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((path, usage)) => {
                out.insert(path, usage);
                pending -= 1;
            }
            Err(_) => {
                eprintln!("warning: {pending} filesystem(s) did not answer within {timeout:?}");
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_blocks_to_bytes() {
        let u = from_blocks(4096, 1000, 400, 300);
        assert_eq!(
            u,
            Usage {
                size: 4_096_000,
                used: 2_457_600,
                avail: 1_228_800
            }
        );
    }

    #[test]
    fn stats_root_and_missing_path() {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let missing = if cfg!(windows) {
            "Q:\\definitely\\not\\here"
        } else {
            "/definitely/not/here"
        };
        let r = stat_all(
            &[root.to_string(), missing.to_string()],
            Duration::from_secs(5),
        );
        assert!(r[root].is_some_and(|u| u.size > 0));
        assert!(r[missing].is_none());
    }
}
