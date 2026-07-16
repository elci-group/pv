//! cgroup v2 freezer backend for atomic process-tree suspend/resume.
//!
//! When the kernel and the active delegation allow it, moving an app's whole
//! process tree into a transient cgroup and writing `cgroup.freeze` is much
//! safer than `SIGSTOP`ing each pid individually:
//!   - the freeze is atomic for the group;
//!   - internal IPC between children keeps working while they are frozen;
//!   - we can later send signals to the whole cgroup with `cgroup.kill`.
//!
//! If cgroup v2 is unavailable, not delegated, or any operation fails, callers
//! fall back to the legacy signal path in [`crate::suspend`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const FREEZE_FILE: &str = "cgroup.freeze";
const PROCS_FILE: &str = "cgroup.procs";
const EVENTS_FILE: &str = "cgroup.events";
const KILL_FILE: &str = "cgroup.kill";
const ESRCH: i32 = 3; // no libc crate dependency

/// A transient cgroup that owns a set of processes and can freeze/thaw them.
#[derive(Debug)]
pub struct Freezer {
    path: PathBuf,
}

impl Freezer {
    /// Create a new transient cgroup for `key` as a child of the current
    /// process's cgroup. Returns an error if cgroup v2 is not usable.
    pub fn new(key: &str) -> Result<Freezer, Error> {
        if !available() {
            return Err(Error::Unavailable);
        }
        let parent = own_cgroup_path()?;
        let name = unique_cgroup_name(key);
        let path = parent.join(&name);
        log::debug!("cgroup: creating {}", path.display());
        fs::create_dir_all(&path).map_err(|e| Error::Create(path.clone(), e))?;

        // Enable the freezer controller for this subtree if we can. Failure
        // here is not fatal — the parent may already have it enabled.
        let _ = enable_freezer(&path);

        Ok(Freezer { path })
    }

    /// Open an existing cgroup path (e.g. one recorded in a SuspendedApp).
    pub fn open(path: PathBuf) -> Result<Freezer, Error> {
        if !path.join(FREEZE_FILE).exists() {
            return Err(Error::Unavailable);
        }
        Ok(Freezer { path })
    }

    /// Path of the cgroup directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Move a process into this cgroup. The caller must already have verified
    /// the pid's identity (start-time) before invoking this.
    pub fn add_pid(&self, pid: u32) -> Result<(), Error> {
        let procs = self.path.join(PROCS_FILE);
        log::trace!("cgroup: moving pid {pid} into {}", self.path.display());
        write_file(&procs, pid.to_string().as_bytes())
            .map_err(|e| Error::AddPid(pid, self.path.clone(), e))
    }

    /// Freeze every process in the cgroup. Waits up to `timeout` for the
    /// kernel to report `frozen 1` in `cgroup.events`.
    pub fn freeze(&self, timeout: Duration) -> Result<(), Error> {
        log::debug!("cgroup: freezing {}", self.path.display());
        write_file(self.path.join(FREEZE_FILE), b"1")
            .map_err(|e| Error::Freeze(self.path.clone(), e))?;
        self.wait_frozen(true, timeout)
            .map_err(|e| Error::Freeze(self.path.clone(), e))
    }

    /// Thaw every process in the cgroup. Waits up to `timeout` for the kernel
    /// to report `frozen 0`.
    pub fn thaw(&self, timeout: Duration) -> Result<(), Error> {
        log::debug!("cgroup: thawing {}", self.path.display());
        write_file(self.path.join(FREEZE_FILE), b"0")
            .map_err(|e| Error::Thaw(self.path.clone(), e))?;
        self.wait_frozen(false, timeout)
            .map_err(|e| Error::Thaw(self.path.clone(), e))
    }

    /// Deliver SIGKILL to every process in the cgroup (kernel 5.14+).
    /// Falls back to returning an error on older kernels so callers can
    /// signal individual pids.
    pub fn kill_all(&self) -> Result<(), Error> {
        let kill_file = self.path.join(KILL_FILE);
        if !kill_file.exists() {
            return Err(Error::NoCgroupKill);
        }
        log::debug!("cgroup: killing {}", self.path.display());
        write_file(&kill_file, b"1").map_err(|e| Error::Kill(self.path.clone(), e))
    }

    /// List the pids currently in this cgroup.
    pub fn members(&self) -> Vec<u32> {
        read_pids(&self.path.join(PROCS_FILE))
    }

    /// Move every process currently in this cgroup back to the parent cgroup,
    /// then delete the transient cgroup. Callers should thaw first if they
    /// want the processes to run; this only handles cgroup bookkeeping.
    pub fn destroy(self) -> Result<(), Error> {
        let parent = match parent_of(&self.path) {
            Some(p) => p,
            None => return Err(Error::Destroy(self.path.clone(), io_error("no parent"))),
        };
        let pids = self.members();
        log::debug!(
            "cgroup: destroying {} (returning {} pids)",
            self.path.display(),
            pids.len()
        );
        let procs = parent.join(PROCS_FILE);
        for pid in &pids {
            if let Err(e) = write_file(&procs, pid.to_string().as_bytes()) {
                // The process may have already exited; that is fine.
                if e.raw_os_error() != Some(ESRCH) && e.kind() != io::ErrorKind::NotFound {
                    log::warn!(
                        "cgroup: could not move pid {pid} back to {}: {e}",
                        parent.display()
                    );
                }
            }
        }
        fs::remove_dir(&self.path)
            .map_err(|e| Error::Destroy(self.path.clone(), e))
    }

    /// Poll `cgroup.events` until `frozen` matches `want` or `timeout` elapses.
    fn wait_frozen(&self, want: bool, timeout: Duration) -> Result<(), io::Error> {
        let events = self.path.join(EVENTS_FILE);
        let deadline = Instant::now() + timeout;
        let want_val = if want { "1" } else { "0" };
        loop {
            if let Some(cur) = read_frozen(&events) {
                if cur == want_val {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "cgroup freeze state did not reach frozen={want_val} in {:?}",
                        timeout
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Errors that can occur when managing a cgroup freezer.
#[derive(Debug)]
pub enum Error {
    Unavailable,
    OwnCgroup(io::Error),
    Create(PathBuf, io::Error),
    AddPid(u32, PathBuf, io::Error),
    Freeze(PathBuf, io::Error),
    Thaw(PathBuf, io::Error),
    Kill(PathBuf, io::Error),
    NoCgroupKill,
    Destroy(PathBuf, io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unavailable => write!(f, "cgroup v2 freezer not available or not delegated"),
            Error::OwnCgroup(e) => write!(f, "cannot read own cgroup: {e}"),
            Error::Create(p, e) => write!(f, "cannot create cgroup {}: {e}", p.display()),
            Error::AddPid(pid, p, e) => {
                write!(f, "cannot move pid {pid} into cgroup {}: {e}", p.display())
            }
            Error::Freeze(p, e) => write!(f, "cannot freeze cgroup {}: {e}", p.display()),
            Error::Thaw(p, e) => write!(f, "cannot thaw cgroup {}: {e}", p.display()),
            Error::Kill(p, e) => write!(f, "cannot kill cgroup {}: {e}", p.display()),
            Error::NoCgroupKill => write!(f, "kernel lacks cgroup.kill (need 5.14+)"),
            Error::Destroy(p, e) => write!(f, "cannot remove cgroup {}: {e}", p.display()),
        }
    }
}

impl std::error::Error for Error {}

/// True when /sys/fs/cgroup looks like a usable cgroup v2 hierarchy with the
/// freezer controller available somewhere above us.
pub fn available() -> bool {
    let events = Path::new(CGROUP_ROOT).join("cgroup.events");
    if !events.exists() {
        return false;
    }
    // The presence of our own cgroup file proves we are actually in the v2
    // hierarchy and not just looking at a stale mount point.
    let own = Path::new("/proc/self/cgroup");
    if !own.exists() {
        return false;
    }
    // Cheap delegation probe: can we read our own cgroup path?
    own_cgroup_path().is_ok()
}

/// Read the v2 cgroup path of the current process from /proc/self/cgroup.
fn own_cgroup_path() -> Result<PathBuf, Error> {
    let text = fs::read_to_string("/proc/self/cgroup")
        .map_err(Error::OwnCgroup)?;
    for line in text.lines() {
        // v2 unified hierarchy lines look like: 0::<path>
        if let Some(rest) = line.strip_prefix("0::") {
            let rel = rest.trim_start_matches('/');
            return if rel.is_empty() {
                Ok(PathBuf::from(CGROUP_ROOT))
            } else {
                Ok(PathBuf::from(CGROUP_ROOT).join(rel))
            };
        }
    }
    Err(Error::OwnCgroup(io_error("no v2 cgroup line found")))
}

/// Enable the freezer controller in a newly created cgroup. This only succeeds
/// when the parent delegated it to us; failure is logged but ignored because
/// the parent may already have freezer enabled for the whole subtree.
fn enable_freezer(path: &Path) -> Result<(), io::Error> {
    let parent = match parent_of(path) {
        Some(p) => p,
        None => return Ok(()),
    };
    let controllers = parent.join("cgroup.subtree_control");
    if !controllers.exists() {
        return Ok(());
    }
    write_file(&controllers, b"+freezer")
}

fn parent_of(path: &Path) -> Option<PathBuf> {
    path.parent().map(Path::to_path_buf)
}

fn write_file(path: impl AsRef<Path>, data: &[u8]) -> Result<(), io::Error> {
    let mut f = fs::File::create(path.as_ref())?;
    f.write_all(data)?;
    f.flush()?;
    Ok(())
}

fn read_frozen(events: &Path) -> Option<String> {
    let text = fs::read_to_string(events).ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("frozen ") {
            return Some(v.to_string());
        }
    }
    None
}

fn read_pids(procs: &Path) -> Vec<u32> {
    fs::read_to_string(procs)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

fn unique_cgroup_name(key: &str) -> String {
    let base: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let base = if base.is_empty() { "pv".into() } else { base };
    format!("pv-{base}-{}", std::process::id())
}

fn io_error(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_name_sanitizes_and_truncates() {
        assert!(unique_cgroup_name("firefox").starts_with("pv-firefox-"));
        assert!(unique_cgroup_name("Web Content (GPU)").starts_with("pv-Web-Content--GPU--"));
        assert!(unique_cgroup_name("").starts_with("pv-pv-"));
        assert!(unique_cgroup_name(&"a/\\*?".repeat(30)).len() <= 60);
    }

    #[test]
    fn read_frozen_parses_events_file() {
        let dir = std::env::temp_dir().join(format!("pv-cgroup-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let events = dir.join("cgroup.events");
        fs::write(&events, "frozen 1\npopulated 0\n").unwrap();
        assert_eq!(read_frozen(&events), Some("1".into()));
        fs::write(&events, "frozen 0\n").unwrap();
        assert_eq!(read_frozen(&events), Some("0".into()));
        fs::write(&events, "populated 1\n").unwrap();
        assert_eq!(read_frozen(&events), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
