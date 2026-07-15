//! Graceful suspension: negotiate, freeze, remember — and resume.

use std::fs;
use std::path::PathBuf;

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

// tiny libc shim — avoids a libc dependency for two syscalls
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
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
                eprintln!("[pv] pid {pid} was recycled since suspend — not signaling");
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
    // graceful phase: give the app a moment by syncing filesystem buffers first
    // (SIGSTOP on a process mid-write is the main source of freeze corruption)
    let _ = std::process::Command::new("sync").status();

    let mut frozen = Vec::new();
    let mut starts = Vec::new();
    for &pid in pids {
        // capture identity before stopping, so resume/kill can detect recycling
        if let Some(start) = start_time(pid) {
            starts.push(PidStart { pid, start });
        }
        if signal(pid, SIGSTOP) {
            frozen.push(pid);
        }
    }
    if frozen.is_empty() {
        return Err("failed to stop any process (permission?)".into());
    }
    let entry = SuspendedApp {
        key: key.to_string(),
        display: display.to_string(),
        pids: pids.to_vec(),
        rss_kb,
        suspended_at: now(),
        task: task.to_string(),
        starts,
    };
    let mut all = load_suspended();
    all.retain(|s| s.key != key);
    all.push(entry.clone());
    if let Err(e) = save_suspended(&all) {
        // roll back: thaw what we froze and record nothing
        thaw(&frozen, &entry.starts);
        return Err(format!("cannot record suspension: {e}"));
    }
    Ok(entry)
}

/// Thaw a previously suspended app. Dead pids are pruned.
pub fn resume(key: &str) -> Result<(usize, u64), String> {
    let mut all = load_suspended();
    let Some(pos) = all.iter().position(|s| s.key == key) else {
        return Err(format!("'{key}' is not suspended"));
    };
    let entry = all.remove(pos);
    let thawed = thaw(&entry.pids, &entry.starts);
    save_suspended(&all).map_err(|e| e.to_string())?;
    Ok((thawed, entry.rss_kb))
}

/// Kill a suspended app entirely (user confirmed).
pub fn kill_suspended(key: &str) -> Result<usize, String> {
    let mut all = load_suspended();
    let Some(pos) = all.iter().position(|s| s.key == key) else {
        return Err(format!("'{key}' is not suspended"));
    };
    let entry = all.remove(pos);
    let mut n = 0;
    for &pid in &entry.pids {
        let known = known_start(&entry.starts, pid);
        signal_guarded(pid, SIGCONT, known); // thaw first so TERM is delivered
        if signal_guarded(pid, SIGTERM, known) {
            n += 1;
        }
    }
    save_suspended(&all).map_err(|e| e.to_string())?;
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
            std::thread::sleep(std::time::Duration::from_millis(5));
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
        };
        let body = toml::to_string_pretty(&SuspendedFile {
            suspended: vec![app.clone()],
        })
        .unwrap();
        let back: SuspendedFile = toml::from_str(&body).unwrap();
        assert_eq!(back.suspended[0].starts, app.starts);
    }
}
