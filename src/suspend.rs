//! Graceful suspension: negotiate, freeze, remember — and resume.
//!
//! This module now prefers cgroup v2 freezing when the kernel and the active
//! delegation allow it. cgroup freezing is atomic for a process tree and keeps
//! internal IPC intact, so it is much safer than `SIGSTOP`ing every pid. If
//! cgroup v2 is unavailable, not delegated, or any step fails, the legacy
//! per-pid `SIGSTOP` path is used as a fallback.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SuspendedApp {
    pub key: String,
    pub display: String,
    pub pids: Vec<u32>,
    pub rss_kb: u64,
    pub suspended_at: u64, // unix secs
    pub task: String,
    // kept last: TOML arrays-of-tables must follow scalar fields;
    // default keeps records written before the guard existed loadable
    #[serde(default)]
    pub starts: Vec<PidStart>,
    // cgroup path used for v2 freezing; None means legacy SIGSTOP suspend
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
}

/// A pid's identity: its start time (field 22 of /proc/<pid>/stat) at suspend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PidStart {
    pub pid: u32,
    pub start: u64,
}

fn state_dir() -> PathBuf {
    crate::procfs::xdg("XDG_DATA_HOME", ".local/share").join("pv")
}

fn state_file() -> PathBuf {
    state_dir().join("suspended.toml")
}

/// TOML documents must be tables — the vec needs a named root key.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SuspendedFile {
    #[serde(default)]
    suspended: Vec<SuspendedApp>,
}

pub fn load_suspended() -> Vec<SuspendedApp> {
    fs::read_to_string(state_file())
        .ok()
        .and_then(|s| toml::from_str::<SuspendedFile>(&s).ok())
        .map(|f| f.suspended)
        .unwrap_or_default()
}

fn save_suspended(v: &[SuspendedApp]) -> std::io::Result<()> {
    fs::create_dir_all(state_dir())?;
    let body = toml::to_string_pretty(&SuspendedFile {
        suspended: v.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    fs::write(state_file(), body)?;
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn signal(pid: u32, sig: i32) -> bool {
    libc_kill(pid as i32, sig) == 0
}

// tiny libc shim — avoids a libc dependency for a few syscalls
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn sync();
}
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}
fn libc_sync() {
    unsafe { sync() };
}

const SIGSTOP: i32 = 19;
const SIGCONT: i32 = 18;
const SIGTERM: i32 = 15;

/// Process start time (field 22 of /proc/<pid>/stat) — parsed from the LAST
/// ')' because comm may contain spaces/parens. None if the pid is gone.
fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 2..].split_whitespace().nth(19)?.parse().ok()
}

fn known_start(starts: &[PidStart], pid: u32) -> Option<u64> {
    starts.iter().find(|s| s.pid == pid).map(|s| s.start)
}

/// Signal a recorded pid only when its current start time still matches the
/// one captured at suspend. A mismatch means the PID was recycled and the
/// signal would hit an innocent process; None (older records) skips the check.
fn signal_guarded(pid: u32, sig: i32, known: Option<u64>) -> bool {
    if let Some(known) = known {
        match start_time(pid) {
            Some(current) if current == known => {}
            Some(_) => {
                log::warn!("pid {pid} was recycled since suspend — not signaling");
                return false;
            }
            None => return false, // already gone
        }
    }
    signal(pid, sig)
}

/// SIGCONT each pid whose identity still checks out. Returns how many thawed.
fn thaw(pids: &[u32], starts: &[PidStart]) -> usize {
    let mut n = 0;
    for &pid in pids {
        if signal_guarded(pid, SIGCONT, known_start(starts, pid)) {
            n += 1;
        }
    }
    n
}

/// Legacy SIGSTOP freeze of all pids. Used when cgroup v2 is unavailable or
/// when moving pids into a cgroup fails.
fn signal_freeze(pids: &[u32]) -> Vec<u32> {
    let mut frozen = Vec::new();
    for &pid in pids {
        if signal(pid, SIGSTOP) {
            frozen.push(pid);
        } else {
            log::warn!("SIGSTOP failed for pid {pid}");
        }
    }
    frozen
}

fn build_entry(
    key: &str,
    display: &str,
    pids: &[u32],
    rss_kb: u64,
    task: &str,
    starts: Vec<PidStart>,
    cgroup: Option<PathBuf>,
) -> SuspendedApp {
    SuspendedApp {
        key: key.to_string(),
        display: display.to_string(),
        pids: pids.to_vec(),
        rss_kb,
        suspended_at: now(),
        task: task.to_string(),
        starts,
        cgroup,
    }
}

fn persist_and_take(entry: SuspendedApp, rollback: impl FnOnce(&SuspendedApp)) -> Result<SuspendedApp, String> {
    let mut all = load_suspended();
    all.retain(|s| s.key != entry.key);
    all.push(entry.clone());
    if let Err(e) = save_suspended(&all) {
        rollback(&entry);
        return Err(format!("cannot record suspension: {e}"));
    }
    Ok(entry)
}

/// Attempt cgroup v2 freezing. On any failure the cgroup is destroyed and
/// processes are returned to their previous cgroup, so the caller can fall
/// back to SIGSTOP.
fn try_cgroup_suspend(
    key: &str,
    display: &str,
    pids: &[u32],
    rss_kb: u64,
    task: &str,
) -> Result<SuspendedApp, String> {
    let freezer = crate::cgroup::Freezer::new(key)
        .map_err(|e| format!("cgroup freezer unavailable: {e}"))?;
    log::info!("{key}: using cgroup freezer at {}", freezer.path().display());

    let mut moved = Vec::new();
    let mut starts = Vec::new();
    for &pid in pids {
        if let Some(start) = start_time(pid) {
            starts.push(PidStart { pid, start });
        }
        if let Err(e) = freezer.add_pid(pid) {
            log::warn!("{key}: cannot move pid {pid} to cgroup: {e}");
            // Roll back any pids we already moved before returning failure.
            let _ = freezer.destroy();
            return Err(format!("cgroup migration failed for pid {pid}: {e}"));
        }
        moved.push(pid);
    }

    if let Err(e) = freezer.freeze(Duration::from_secs(2)) {
        log::warn!("{key}: cgroup freeze failed: {e}");
        let _ = freezer.destroy();
        return Err(format!("cgroup freeze failed: {e}"));
    }

    let entry = build_entry(
        key,
        display,
        pids,
        rss_kb,
        task,
        starts,
        Some(freezer.path().to_path_buf()),
    );
    // Do not destroy the freezer here: the cgroup holds the frozen processes
    // and must survive until resume or kill.
    persist_and_take(entry, |e| {
        log::warn!("{key}: recording suspension failed, thawing cgroup");
        if let Some(path) = &e.cgroup {
            if let Ok(f) = crate::cgroup::Freezer::open(path.clone()) {
                let _ = f.thaw(Duration::from_secs(2));
                let _ = f.destroy();
            }
        }
    })
}

/// Freeze all pids of an app and record it. Returns the recorded entry.
pub fn suspend(
    key: &str,
    display: &str,
    pids: &[u32],
    rss_kb: u64,
    task: &str,
) -> Result<SuspendedApp, String> {
    if pids.is_empty() {
        return Err("no processes matched".into());
    }
    // graceful phase: sync filesystem buffers before stopping so that processes
    // mid-write are less likely to leave corrupt state
    libc_sync();

    // Prefer cgroup v2 freezing when available.
    match try_cgroup_suspend(key, display, pids, rss_kb, task) {
        Ok(entry) => return Ok(entry),
        Err(e) => log::info!("{key}: falling back to SIGSTOP: {e}"),
    }

    // Legacy per-pid SIGSTOP fallback.
    let starts: Vec<PidStart> = pids
        .iter()
        .filter_map(|&pid| start_time(pid).map(|start| PidStart { pid, start }))
        .collect();
    let frozen = signal_freeze(pids);
    if frozen.is_empty() {
        return Err("failed to stop any process (permission?)".into());
    }
    let entry = build_entry(key, display, pids, rss_kb, task, starts, None);
    persist_and_take(entry, |e| {
        log::warn!("{key}: recording suspension failed, thawing {n} pids", n = e.pids.len());
        thaw(&e.pids, &e.starts);
    })
}

/// Thaw a previously suspended app. Dead pids are pruned.
pub fn resume(key: &str) -> Result<(usize, u64), String> {
    let mut all = load_suspended();
    let Some(pos) = all.iter().position(|s| s.key == key) else {
        return Err(format!("'{key}' is not suspended"));
    };
    let entry = all.remove(pos);

    let thawed = if let Some(path) = &entry.cgroup {
        match resume_cgroup(path) {
            Ok(n) => {
                log::info!("{key}: thawed {n} processes from cgroup");
                n
            }
            Err(e) => {
                log::warn!("{key}: cgroup thaw failed ({e}), falling back to SIGCONT");
                thaw(&entry.pids, &entry.starts)
            }
        }
    } else {
        thaw(&entry.pids, &entry.starts)
    };

    save_suspended(&all).map_err(|e| e.to_string())?;
    Ok((thawed, entry.rss_kb))
}

fn resume_cgroup(path: &PathBuf) -> Result<usize, String> {
    let freezer = crate::cgroup::Freezer::open(path.clone())
        .map_err(|e| format!("cannot open cgroup: {e}"))?;
    let n = freezer.members().len();
    freezer
        .thaw(Duration::from_secs(2))
        .map_err(|e| format!("thaw failed: {e}"))?;
    freezer
        .destroy()
        .map_err(|e| format!("destroy failed: {e}"))?;
    Ok(n)
}

/// Kill a suspended app entirely (user confirmed).
pub fn kill_suspended(key: &str) -> Result<usize, String> {
    let mut all = load_suspended();
    let Some(pos) = all.iter().position(|s| s.key == key) else {
        return Err(format!("'{key}' is not suspended"));
    };
    let entry = all.remove(pos);

    let n = if let Some(path) = &entry.cgroup {
        match kill_cgroup(path, &entry.starts) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("{key}: cgroup kill failed ({e}), falling back to signals");
                signal_kill(&entry.pids, &entry.starts)
            }
        }
    } else {
        signal_kill(&entry.pids, &entry.starts)
    };

    save_suspended(&all).map_err(|e| e.to_string())?;
    Ok(n)
}

fn signal_kill(pids: &[u32], starts: &[PidStart]) -> usize {
    let mut n = 0;
    for &pid in pids {
        let known = known_start(starts, pid);
        signal_guarded(pid, SIGCONT, known); // thaw first so TERM is delivered
        if signal_guarded(pid, SIGTERM, known) {
            n += 1;
        }
    }
    n
}

fn kill_cgroup(path: &PathBuf, starts: &[PidStart]) -> Result<usize, String> {
    let freezer = crate::cgroup::Freezer::open(path.clone())
        .map_err(|e| format!("cannot open cgroup: {e}"))?;
    // Capture members while still frozen so they cannot fork while we decide
    // whom to signal. SIGTERM is not delivered until we thaw.
    let pids = freezer.members();
    if let Err(e) = freezer.thaw(Duration::from_secs(2)) {
        log::warn!("cgroup thaw before kill failed: {e}");
    }
    let mut n = 0;
    for &pid in &pids {
        let known = known_start(starts, pid);
        signal_guarded(pid, SIGCONT, known); // ensure TERM can be delivered
        if signal_guarded(pid, SIGTERM, known) {
            n += 1;
        }
    }
    // Give processes a moment to exit before destroying the cgroup.
    thread::sleep(Duration::from_millis(200));
    if let Err(e) = freezer.destroy() {
        log::warn!("cgroup destroy after kill failed: {e}");
    }
    Ok(n)
}

/// Drop records whose pids have all vanished.
pub fn gc() {
    let mut all = load_suspended();
    all.retain(|s| {
        s.pids
            .iter()
            .any(|p| PathBuf::from(format!("/proc/{p}")).exists())
    });
    let _ = save_suspended(&all);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_state(pid: u32) -> Option<char> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rfind(')')
            .and_then(|close| stat[close + 2..].chars().next())
    }

    fn wait_state(pid: u32, want: char) -> bool {
        for _ in 0..100 {
            if proc_state(pid) == Some(want) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn start_time_reads_field_22() {
        let pid = std::process::id();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let close = stat.rfind(')').unwrap();
        let field22: u64 = stat[close + 2..]
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(start_time(pid), Some(field22));
    }

    #[test]
    fn start_time_none_for_dead_pid() {
        assert_eq!(start_time(u32::MAX), None);
    }

    #[test]
    fn guard_signals_only_matching_identity() {
        let pid = std::process::id();
        let start = start_time(pid).unwrap();
        // null signal 0 — checks the guard logic without really signaling
        assert!(signal_guarded(pid, 0, Some(start)));
        assert!(signal_guarded(pid, 0, None)); // unknown start: unchecked (old records)
        assert!(!signal_guarded(pid, 0, Some(start.wrapping_add(1)))); // recycled
        assert!(!signal_guarded(u32::MAX, 0, Some(1))); // gone
    }

    #[test]
    fn thaw_rolls_back_only_verified_pids() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let start = start_time(pid).expect("child start time");
        assert!(signal(pid, SIGSTOP));
        assert!(wait_state(pid, 'T'));
        // wrong recorded start (as if recycled) — must stay frozen and unsignaled
        assert_eq!(
            thaw(
                &[pid],
                &[PidStart {
                    pid,
                    start: start.wrapping_add(1)
                }]
            ),
            0
        );
        assert_eq!(proc_state(pid), Some('T'));
        // correct identity — thawed (this is also the suspend rollback path)
        assert_eq!(thaw(&[pid], &[PidStart { pid, start }]), 1);
        assert!(wait_state(pid, 'S'));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn records_without_starts_still_load() {
        let doc = r#"
[[suspended]]
key = "chrome"
display = "Chrome"
pids = [1234]
rss_kb = 102400
suspended_at = 1700000000
task = "browsing"
"#;
        let f: SuspendedFile = toml::from_str(doc).unwrap();
        assert_eq!(f.suspended.len(), 1);
        assert!(f.suspended[0].starts.is_empty());
        assert_eq!(known_start(&f.suspended[0].starts, 1234), None);
        assert!(f.suspended[0].cgroup.is_none());
    }

    #[test]
    fn starts_round_trip_through_toml() {
        let app = SuspendedApp {
            key: "chrome".into(),
            display: "Chrome".into(),
            pids: vec![1234],
            rss_kb: 102400,
            suspended_at: 1_700_000_000,
            task: "browsing".into(),
            starts: vec![PidStart {
                pid: 1234,
                start: 987_654_321,
            }],
            cgroup: Some(PathBuf::from("/sys/fs/cgroup/pv-chrome-42")),
        };
        let body = toml::to_string_pretty(&SuspendedFile {
            suspended: vec![app.clone()],
        })
        .unwrap();
        let back: SuspendedFile = toml::from_str(&body).unwrap();
        assert_eq!(back.suspended[0].starts, app.starts);
        assert_eq!(back.suspended[0].cgroup, app.cgroup);
    }
}
