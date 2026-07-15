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
    let body = toml::to_string_pretty(&SuspendedFile { suspended: v.to_vec() })
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

/// Freeze all pids of an app and record it. Returns the recorded entry.
pub fn suspend(key: &str, display: &str, pids: &[u32], rss_kb: u64, task: &str) -> Result<SuspendedApp, String> {
    if pids.is_empty() {
        return Err("no processes matched".into());
    }
    // graceful phase: give the app a moment by syncing filesystem buffers first
    // (SIGSTOP on a process mid-write is the main source of freeze corruption)
    let _ = std::process::Command::new("sync").status();

    let mut frozen = 0;
    for &pid in pids {
        if signal(pid, SIGSTOP) {
            frozen += 1;
        }
    }
    if frozen == 0 {
        return Err("failed to stop any process (permission?)".into());
    }
    let entry = SuspendedApp {
        key: key.to_string(),
        display: display.to_string(),
        pids: pids.to_vec(),
        rss_kb,
        suspended_at: now(),
        task: task.to_string(),
    };
    let mut all = load_suspended();
    all.retain(|s| s.key != key);
    all.push(entry.clone());
    save_suspended(&all).map_err(|e| e.to_string())?;
    Ok(entry)
}

/// Thaw a previously suspended app. Dead pids are pruned.
pub fn resume(key: &str) -> Result<(usize, u64), String> {
    let mut all = load_suspended();
    let Some(pos) = all.iter().position(|s| s.key == key) else {
        return Err(format!("'{key}' is not suspended"));
    };
    let entry = all.remove(pos);
    let mut thawed = 0;
    for &pid in &entry.pids {
        if signal(pid, SIGCONT) {
            thawed += 1;
        }
    }
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
        signal(pid, SIGCONT); // thaw first so TERM is delivered
        if signal(pid, SIGTERM) {
            n += 1;
        }
    }
    save_suspended(&all).map_err(|e| e.to_string())?;
    Ok(n)
}

/// Drop records whose pids have all vanished.
pub fn gc() {
    let mut all = load_suspended();
    all.retain(|s| s.pids.iter().any(|p| PathBuf::from(format!("/proc/{p}")).exists()));
    let _ = save_suspended(&all);
}
