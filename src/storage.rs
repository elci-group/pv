//! Storage reporting: per-filesystem capacity via statvfs over /proc/mounts.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;

/// glibc `struct statvfs` as laid out on 64-bit Linux (x86_64/aarch64):
/// eleven u64 fields plus reserved padding, 112 bytes total. pv releases
/// target 64-bit Linux only.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // FFI output buffer; libc fills fields we do not all read
struct Statvfs {
    bsize: u64,
    frsize: u64,
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    favail: u64,
    fsid: u64,
    flag: u64,
    namemax: u64,
    spare: [i32; 6],
}

extern "C" {
    fn statvfs(path: *const std::ffi::c_char, buf: *mut Statvfs) -> i32;
}

fn statvfs_of(mount: &str) -> Option<Statvfs> {
    let c = CString::new(mount).ok()?;
    let mut buf = Statvfs::default();
    // SAFETY: `c` is a valid NUL-terminated string and `buf` is a properly
    // sized, aligned Statvfs for libc to fill in.
    (unsafe { statvfs(c.as_ptr(), &mut buf) } == 0).then_some(buf)
}

/// One mounted filesystem's capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filesystem {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total_kb: u64,
    pub avail_kb: u64, // free to unprivileged users (f_bavail)
    pub used_pct: u8,
}

/// Parse /proc/mounts into (device, mount, fstype) triples.
fn parse_mounts(s: &str) -> Vec<(String, String, String)> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let device = it.next()?;
            let mount = it.next()?;
            let fstype = it.next()?;
            Some((device.to_string(), unescape(mount), fstype.to_string()))
        })
        .collect()
}

/// The kernel escapes ' ', '\t', '\n' and '\\' in mount paths as \ooo octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|d| (b'0'..=b'7').contains(d)) {
                out.push((oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0'));
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pseudo filesystems that report real block counts but hold no
/// user-reclaimable capacity, loop devices (snap squashfs), and container
/// overlays, which only restate their host filesystem — all noise in a
/// pressure report.
fn keep(device: &str, fstype: &str) -> bool {
    const PSEUDO: &[&str] = &[
        "bpf",
        "configfs",
        "debugfs",
        "efivarfs",
        "fusectl",
        "hugetlbfs",
        "mqueue",
        "pstore",
        "rpc_pipefs",
        "securityfs",
        "tracefs",
    ];
    !device.starts_with("/dev/loop") && fstype != "overlay" && !PSEUDO.contains(&fstype)
}

/// df semantics: reserved blocks count as used, so a filesystem can read
/// 100% while f_bavail still shows space to root.
fn calc_used_pct(blocks: u64, bfree: u64, bavail: u64) -> u8 {
    let used = blocks.saturating_sub(bfree);
    let denom = used.saturating_add(bavail);
    if denom == 0 {
        return 0;
    }
    (used as u128 * 100 / denom as u128).min(100) as u8
}

fn to_kb(count: u64, frsize: u64) -> u64 {
    ((count as u128 * frsize as u128) / 1024).min(u64::MAX as u128) as u64
}

/// Bind mounts repeat the same block device at several paths — keep the
/// shortest mount path per device. Non-device sources (tmpfs, nfs) are
/// distinct per mount and never deduped.
fn dedupe(fs: Vec<Filesystem>) -> Vec<Filesystem> {
    let mut by_key: HashMap<String, Filesystem> = HashMap::new();
    for f in fs {
        let key = if f.device.starts_with("/dev/") {
            f.device.clone()
        } else {
            f.mount.clone()
        };
        by_key
            .entry(key)
            .and_modify(|e| {
                if f.mount.len() < e.mount.len() {
                    *e = f.clone();
                }
            })
            .or_insert(f);
    }
    by_key.into_values().collect()
}

/// Real mounted filesystems, fullest first.
pub fn filesystems() -> Vec<Filesystem> {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let out: Vec<Filesystem> = parse_mounts(&mounts)
        .into_iter()
        .filter(|(device, _, fstype)| keep(device, fstype))
        .filter_map(|(device, mount, fstype)| {
            let sv = statvfs_of(&mount)?;
            if sv.blocks == 0 {
                return None; // pseudo filesystem (proc, sysfs, cgroup, ...)
            }
            Some(Filesystem {
                mount,
                device,
                fstype,
                total_kb: to_kb(sv.blocks, sv.frsize),
                avail_kb: to_kb(sv.bavail, sv.frsize),
                used_pct: calc_used_pct(sv.blocks, sv.bfree, sv.bavail),
            })
        })
        .collect();
    let mut out = dedupe(out);
    out.sort_by(|a, b| {
        b.used_pct
            .cmp(&a.used_pct)
            .then_with(|| a.mount.cmp(&b.mount))
    });
    out
}

/// Fullest filesystem of a `filesystems()` listing (already sorted).
pub fn fullest(fs: &[Filesystem]) -> Option<&Filesystem> {
    fs.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mounts_reads_device_mount_fstype() {
        let s = "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
                 /dev/nvme0n1p2 / ext4 rw,relatime 0 0\n\
                 tmpfs /tmp tmpfs rw,nosuid,nodev 0 0\n";
        let m = parse_mounts(s);
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[1],
            (
                "/dev/nvme0n1p2".to_string(),
                "/".to_string(),
                "ext4".to_string()
            )
        );
    }

    #[test]
    fn parse_mounts_unescapes_octal_sequences() {
        let s = "/dev/sda1 /mnt/my\\040drive ext4 rw 0 0\n\
                 /dev/sdb1 /mnt/tab\\011here ext4 rw 0 0\n";
        let m = parse_mounts(s);
        assert_eq!(m[0].1, "/mnt/my drive");
        assert_eq!(m[1].1, "/mnt/tab\there");
    }

    #[test]
    fn parse_mounts_skips_malformed_lines() {
        assert!(parse_mounts("").is_empty());
        assert!(parse_mounts("only-one-field\n").is_empty());
        assert_eq!(parse_mounts("a b c\nd\n").len(), 1);
    }

    #[test]
    fn keep_drops_loop_overlay_and_pseudo_noise() {
        assert!(!keep("/dev/loop0", "squashfs"));
        assert!(!keep("/dev/loop23", "ext4"));
        assert!(!keep("overlay", "overlay"));
        assert!(!keep("efivarfs", "efivarfs"));
        assert!(!keep("none", "pstore"));
        assert!(keep("/dev/nvme0n1p2", "ext4"));
        assert!(keep("tmpfs", "tmpfs"));
        assert!(keep("udev", "devtmpfs"));
        assert!(keep("server:/export", "nfs"));
    }

    #[test]
    fn used_pct_counts_reserved_blocks_as_used() {
        // blocks 100, bfree 10, bavail 5 -> used 90, 90/(90+5) = 94%
        assert_eq!(calc_used_pct(100, 10, 5), 94);
        // nothing used even though root reserves blocks
        assert_eq!(calc_used_pct(100, 100, 95), 0);
    }

    #[test]
    fn used_pct_handles_empty_and_full() {
        assert_eq!(calc_used_pct(0, 0, 0), 0, "pseudo fs, no blocks");
        assert_eq!(calc_used_pct(100, 0, 0), 100);
        assert_eq!(calc_used_pct(200, 100, 100), 50);
    }

    fn fs(mount: &str, device: &str, fstype: &str, used_pct: u8) -> Filesystem {
        Filesystem {
            mount: mount.to_string(),
            device: device.to_string(),
            fstype: fstype.to_string(),
            total_kb: 1000,
            avail_kb: 500,
            used_pct,
        }
    }

    #[test]
    fn dedupe_keeps_shortest_mount_per_block_device() {
        let out = dedupe(vec![
            fs("/var/lib/docker/bind", "/dev/sda1", "ext4", 50),
            fs("/", "/dev/sda1", "ext4", 50),
            fs("/boot", "/dev/sda2", "vfat", 10),
        ]);
        assert_eq!(out.len(), 2);
        let root = out.iter().find(|f| f.device == "/dev/sda1").unwrap();
        assert_eq!(root.mount, "/");
    }

    #[test]
    fn dedupe_keeps_distinct_tmpfs_mounts() {
        let out = dedupe(vec![
            fs("/tmp", "tmpfs", "tmpfs", 5),
            fs("/dev/shm", "tmpfs", "tmpfs", 1),
        ]);
        assert_eq!(out.len(), 2, "same source name, distinct instances");
    }
}
