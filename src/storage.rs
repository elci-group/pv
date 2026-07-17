//! Storage reporting: per-filesystem capacity via statvfs over /proc/mounts.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;

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

// ---------- reclaim analysis (deckhand) ----------

/// Fullest-filesystem use% that activates reclaim analysis.
pub const PRESSURE_PCT: u8 = 85;
/// How long a cached scan stays fresh; mirrors kaptaind's deckhand interval.
const SCAN_MAX_AGE_SECS: u64 = 6 * 3600;
/// A .tmp older than this means the scanner died; allow a respawn.
const TMP_MAX_AGE_SECS: u64 = 30 * 60;

pub fn under_pressure(used_pct: u8) -> bool {
    used_pct >= PRESSURE_PCT
}

/// Distilled deckhand inspect result, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaim {
    pub total_human: String,
    pub candidates: u64,
    pub projects: Vec<(String, String)>, // (name, cleanable_human), top 5
}

#[derive(Debug)]
pub enum ReclaimState {
    Fresh(Reclaim, u64), // cache age in seconds
    Scanning,            // first scan in flight, nothing cached yet
    Dark,                // no deckhand or no data — render nothing
}

fn cache_paths() -> (PathBuf, PathBuf) {
    let dir = crate::procfs::xdg("XDG_DATA_HOME", ".local/share").join("pv");
    (
        dir.join("deckhand_scan.json"),
        dir.join("deckhand_scan.json.tmp"),
    )
}

fn file_age_secs(p: &Path) -> Option<u64> {
    let mtime = fs::metadata(p).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|d| d.as_secs())
}

/// Extract a string value from one `"key": "value"` line of pretty-printed
/// JSON, unescaping the standard escapes. Escaped quotes inside values can
/// never false-match another key, because they are always backslash-prefixed.
fn json_str_value(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let pos = line.find(&pat)?;
    let rest = line[pos + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => return None,
            },
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

/// Extract an unsigned integer value from one `"key": N` line.
fn json_num_value(line: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\"");
    let pos = line.find(&pat)?;
    let rest = line[pos + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Distill deckhand's pretty-printed inspect JSON down to what pv renders.
/// Line-oriented by design — no JSON crate — and strict: anything malformed
/// (including a truncated .tmp mid-write) yields None and is ignored.
fn distill(json: &str) -> Option<Reclaim> {
    // a complete deckhand report ends with the closing brace; a partial
    // write never does
    if !json.trim_end().ends_with('}') {
        return None;
    }
    let mut total_human = None;
    let mut candidates = None;
    let mut projects = Vec::new();
    let mut in_projects = false;
    let mut current_name: Option<String> = None;
    for line in json.lines() {
        if !in_projects {
            if total_human.is_none() {
                total_human = json_str_value(line, "total_cleanable_human");
            }
            if candidates.is_none() {
                candidates = json_num_value(line, "candidates");
            }
            if line.contains("\"projects\"") {
                in_projects = true;
            }
            continue;
        }
        if projects.len() >= 5 {
            break;
        }
        if let Some(name) = json_str_value(line, "name") {
            current_name = Some(name);
        } else if let Some(human) = json_str_value(line, "cleanable_human") {
            projects.push((current_name.take()?, human));
        }
    }
    Some(Reclaim {
        total_human: total_human?,
        candidates: candidates?,
        projects,
    })
}

/// Where deckhand looks for reclaimable projects: user projects live under
/// $HOME, so scan there when the pressured filesystem backs it; otherwise
/// scan the pressured mount itself.
fn scan_root(fullest_mount: &str, home: &str) -> String {
    let mount = fullest_mount.trim_end_matches('/');
    if mount.is_empty() || home == mount || home.starts_with(&format!("{mount}/")) {
        home.to_string()
    } else {
        fullest_mount.to_string()
    }
}

/// Spawn only under pressure, only when the cache is stale, and never while
/// a scan is already running.
fn should_spawn(pressure: bool, cache_age: Option<u64>, tmp_age: Option<u64>) -> bool {
    if !pressure {
        return false;
    }
    let cache_stale = cache_age.is_none_or(|a| a > SCAN_MAX_AGE_SECS);
    let tmp_live = tmp_age.is_some_and(|a| a <= TMP_MAX_AGE_SECS);
    cache_stale && !tmp_live
}

/// Launch `deckhand inspect` detached, writing the .tmp cache. Never waited
/// on — the scan takes over a minute on a large home directory. The
/// create_new open makes concurrent pv runs single-winner: the loser sees
/// the live .tmp and reports Scanning instead of double-spawning.
fn spawn_scan(tmp: &Path, root: &str) {
    if let Some(parent) = tmp.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
    else {
        return;
    };
    let spawned = std::process::Command::new("deckhand")
        .args(["inspect", "--json", "--min-size", "100MB", "--path", root])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(file)
        .spawn();
    if spawned.is_err() {
        let _ = fs::remove_file(tmp);
    }
}

/// Reclaim analysis for the current storage state. Never blocks on deckhand:
/// crossing the pressure threshold spawns the scan in the background and
/// every call renders the last finished scan from cache.
pub fn reclaim(used_pct: u8, fullest_mount: &str) -> ReclaimState {
    if !crate::doctor::on_path("deckhand") {
        return ReclaimState::Dark;
    }
    let (cache, tmp) = cache_paths();
    // a .tmp that distills is a finished scan — promote it atomically
    if let Ok(body) = fs::read_to_string(&tmp) {
        if distill(&body).is_some() {
            let _ = fs::rename(&tmp, &cache);
        }
    }
    let have = fs::read_to_string(&cache)
        .ok()
        .and_then(|body| distill(&body).map(|r| (r, file_age_secs(&cache))));
    let pressure = under_pressure(used_pct);
    let tmp_age = file_age_secs(&tmp);
    if should_spawn(pressure, have.as_ref().and_then(|(_, a)| *a), tmp_age) {
        let _ = fs::remove_file(&tmp); // dead scanner, if one left a .tmp
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        spawn_scan(&tmp, &scan_root(fullest_mount, &home));
        return ReclaimState::Scanning;
    }
    match have {
        Some((r, age)) => ReclaimState::Fresh(r, age.unwrap_or(0)),
        None if pressure && tmp_age.is_some() => ReclaimState::Scanning,
        _ => ReclaimState::Dark,
    }
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

    // ---------- reclaim analysis ----------

    const DECKHAND_JSON: &str = r#"{
  "scan_root": "/home/sal",
  "total_projects": 2862,
  "candidates": 2,
  "total_cleanable_bytes": 29426547225,
  "total_cleanable_human": "27.40 GB",
  "min_size_bytes": 104857600,
  "projects": [
    {
      "name": "kaptaind",
      "path": "/home/sal/kaptaind",
      "target": "/home/sal/kaptaind/target",
      "candidate": true,
      "cleanable_bytes": 19494638539,
      "cleanable_human": "18.16 GB",
      "debug_bytes": 18568681574,
      "release_bytes": 863382977
    },
    {
      "name": "grow",
      "path": "/home/sal/grow",
      "target": "/home/sal/grow/target",
      "candidate": true,
      "cleanable_bytes": 9931933016,
      "cleanable_human": "9.25 GB",
      "debug_bytes": 8390148702,
      "release_bytes": 123330001
    }
  ]
}
"#;

    #[test]
    fn distill_reads_totals_and_projects() {
        let r = distill(DECKHAND_JSON).expect("valid report");
        assert_eq!(r.total_human, "27.40 GB");
        assert_eq!(r.candidates, 2);
        assert_eq!(
            r.projects,
            vec![
                ("kaptaind".to_string(), "18.16 GB".to_string()),
                ("grow".to_string(), "9.25 GB".to_string()),
            ]
        );
    }

    #[test]
    fn distill_caps_projects_at_five() {
        let mut json = String::from(
            "{\n  \"candidates\": 7,\n  \"total_cleanable_human\": \"9 GB\",\n  \"projects\": [\n",
        );
        for i in 0..7 {
            json.push_str(&format!(
                "    {{\n      \"name\": \"p{i}\",\n      \"cleanable_human\": \"{i} GB\"\n    }},\n"
            ));
        }
        json.push_str("  ]\n}\n");
        let r = distill(&json).expect("valid report");
        assert_eq!(r.projects.len(), 5);
        assert_eq!(r.projects[4].0, "p4");
    }

    #[test]
    fn distill_unescapes_project_names() {
        let json = "{\n  \"candidates\": 1,\n  \"total_cleanable_human\": \"1 GB\",\n  \
                    \"projects\": [\n    {\n      \"name\": \"we\\\"ird \\\\ proj\",\n      \
                    \"cleanable_human\": \"1 GB\"\n    }\n  ]\n}\n";
        let r = distill(json).expect("valid report");
        assert_eq!(r.projects[0].0, "we\"ird \\ proj");
    }

    #[test]
    fn distill_rejects_truncated_and_malformed() {
        // cut mid-write: no closing brace, top fields incomplete
        let cut = &DECKHAND_JSON[..DECKHAND_JSON.len() / 2];
        assert_eq!(distill(cut), None);
        assert_eq!(distill(""), None);
        assert_eq!(distill("garbage"), None);
        // closing brace but missing required totals
        assert_eq!(distill("{ \"projects\": [] }\n"), None);
    }

    #[test]
    fn scan_root_prefers_home_on_its_own_filesystem() {
        assert_eq!(scan_root("/", "/home/sal"), "/home/sal");
        assert_eq!(scan_root("/home", "/home/sal"), "/home/sal");
        assert_eq!(scan_root("/home/sal", "/home/sal"), "/home/sal");
    }

    #[test]
    fn scan_root_uses_pressured_mount_when_home_is_elsewhere() {
        assert_eq!(scan_root("/data", "/home/sal"), "/data");
        assert_eq!(scan_root("/mnt/disk2", "/home/sal"), "/mnt/disk2");
    }

    #[test]
    fn pressure_threshold_is_85_percent() {
        assert!(!under_pressure(84));
        assert!(under_pressure(85));
        assert!(under_pressure(100));
    }

    #[test]
    fn spawn_only_under_pressure_with_stale_cache_and_no_live_scan() {
        let fresh = Some(60); // 1 min old cache
        let stale = Some(7 * 3600); // 7 h old cache
        let live_tmp = Some(5 * 60); // scanner started 5 min ago
        let dead_tmp = Some(60 * 60); // .tmp abandoned an hour ago

        assert!(!should_spawn(false, None, None), "no pressure, no scan");
        assert!(!should_spawn(true, fresh, None), "cache still fresh");
        assert!(!should_spawn(true, stale, live_tmp), "scan already running");
        assert!(should_spawn(true, stale, dead_tmp), "dead scanner, restart");
        assert!(should_spawn(true, None, None), "first scan under pressure");
    }
}
