//! /proc and /sys parsing: processes, memory, PSI, battery, thermals.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// Clock ticks per second (USER_HZ), probed once via `getconf CLK_TCK`.
/// Falls back to 100.0, the value on virtually all Linux systems.
pub fn ticks_per_sec() -> f64 {
    static TPS: OnceLock<f64> = OnceLock::new();
    *TPS.get_or_init(|| {
        std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(100.0)
    })
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // full /proc record; not every field is rendered yet
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub state: char,
    pub tty: bool,
    pub comm: String,
    pub exe: String,       // basename of executable
    pub cmdline: String,   // full command line, space-joined
    pub argv: Vec<String>, // raw argv, NUL-separated in /proc
    pub uid: u32,
    pub rss_kb: u64,
    pub threads: u32,
    pub utime: u64, // ticks
    pub stime: u64, // ticks
    pub age_secs: f64,
    pub has_audio: bool, // holds an open sound device
    pub kernel_thread: bool,
}

impl Process {
    pub fn cpu_ticks(&self) -> u64 {
        self.utime + self.stime
    }
}

/// A group of processes belonging to one logical application.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct App {
    pub key: String,     // normalized app key, e.g. "firefox"
    pub display: String, // display name, e.g. "Firefox"
    pub pids: Vec<u32>,
    pub leader: u32, // oldest / primary pid
    pub rss_kb: u64,
    pub cpu_pct: f64, // sampled aggregate cpu%
    pub state: char,  // "best" state across members
    pub tty: bool,
    pub has_audio: bool,
    pub cmdline: String,   // representative cmdline (leader's)
    pub argv: Vec<String>, // leader's raw argv
    pub age_secs: f64,
    pub kernel: bool,
}

fn read_to_string(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok()
}

/// XDG base dir; per spec an empty value means "unset", so fall back then too.
pub fn xdg(var: &str, default_suffix: &str) -> std::path::PathBuf {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(default_suffix)
        })
}

pub fn list_pids() -> Vec<u32> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(n) = e.file_name().to_string_lossy().parse::<u32>() {
                out.push(n);
            }
        }
    }
    out
}

pub fn system_uptime() -> f64 {
    read_to_string(Path::new("/proc/uptime"))
        .and_then(|s| s.split_whitespace().next().map(str::to_owned))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

pub fn read_process(pid: u32, uptime: f64) -> Option<Process> {
    let stat = read_to_string(Path::new(&format!("/proc/{pid}/stat")))?;
    // comm may contain spaces/parens — parse from the LAST ')'
    let close = stat.rfind(')')?;
    let comm = stat[stat.find('(')? + 1..close].to_string();
    let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
    if f.len() < 22 {
        return None;
    }
    // f[0] is field 3 (state)
    let state = f[0].chars().next().unwrap_or('?');
    let ppid: u32 = f[1].parse().ok()?;
    let pgrp: u32 = f[2].parse().ok()?;
    let tty_nr: i32 = f[4].parse().unwrap_or(0);
    let utime: u64 = f[11].parse().unwrap_or(0);
    let stime: u64 = f[12].parse().unwrap_or(0);
    let starttime: u64 = f[19].parse().unwrap_or(0);

    let cmdline_raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let kernel_thread = cmdline_raw.is_empty();
    let argv: Vec<String> = cmdline_raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let cmdline = argv.join(" ");

    let mut uid = 0u32;
    let mut rss_kb = 0u64;
    let mut threads = 1u32;
    if let Some(status) = read_to_string(Path::new(&format!("/proc/{pid}/status"))) {
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("Uid:") {
                uid = v.split_whitespace().next()?.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("VmRSS:") {
                rss_kb = v.split_whitespace().next()?.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("Threads:") {
                threads = v.trim().parse().unwrap_or(1);
            }
        }
    }

    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|n| n.trim_end_matches(" (deleted)").to_string())
        .unwrap_or_else(|| comm.clone());

    // audio heuristic: holds an open /dev/snd node
    let has_audio = fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|rd| {
            rd.flatten().any(|e| {
                fs::read_link(e.path())
                    .map(|t| t.to_string_lossy().contains("/dev/snd"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    Some(Process {
        pid,
        ppid,
        pgrp,
        state,
        tty: tty_nr != 0,
        comm,
        exe,
        cmdline,
        argv,
        uid,
        rss_kb,
        threads,
        utime,
        stime,
        age_secs: (uptime - starttime as f64 / ticks_per_sec()).max(0.0),
        has_audio,
        kernel_thread,
    })
}

/// Snapshot all user-visible processes twice, `sample_ms` apart, to compute live CPU%.
pub fn snapshot(sample_ms: u64) -> Vec<Process> {
    let uptime = system_uptime();
    let pids = list_pids();
    let first: HashMap<u32, u64> = pids
        .iter()
        .filter_map(|&p| read_process(p, uptime).map(|pr| (p, pr.cpu_ticks())))
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(sample_ms));
    let mut out = Vec::new();
    for &p in &pids {
        if let Some(mut pr) = read_process(p, uptime) {
            if let Some(&t0) = first.get(&p) {
                // stash delta ticks in a way callers can turn into %
                pr.utime = pr.cpu_ticks().saturating_sub(t0);
                pr.stime = 0;
            }
            out.push(pr);
        }
    }
    out
}

/// Group processes into logical applications and attach sampled CPU%.
pub fn group_apps(mut procs: Vec<Process>, sample_ms: u64) -> Vec<App> {
    let mut groups: HashMap<String, Vec<Process>> = HashMap::new();
    for p in procs.drain(..) {
        if p.kernel_thread || p.pid <= 1 {
            continue;
        }
        let key = p.exe.to_lowercase();
        groups.entry(key).or_default().push(p);
    }
    let secs = sample_ms as f64 / 1000.0;
    let mut apps: Vec<App> = groups
        .into_iter()
        .map(|(key, members)| {
            let rss_kb = members.iter().map(|m| m.rss_kb).sum();
            let cpu_pct: f64 = members
                .iter()
                .map(|m| m.utime as f64 / ticks_per_sec() / secs * 100.0)
                .sum();
            let leader = members
                .iter()
                .max_by(|a, b| a.age_secs.partial_cmp(&b.age_secs).unwrap())
                .unwrap();
            let display = pretty_name(&key);
            let state = if members.iter().any(|m| m.state == 'R') {
                'R'
            } else if members.iter().any(|m| m.state == 'S') {
                'S'
            } else if members.iter().any(|m| m.state == 'T') {
                'T'
            } else if members.iter().any(|m| m.state == 'D') {
                'D'
            } else {
                members[0].state
            };
            App {
                key: key.clone(),
                display,
                pids: members.iter().map(|m| m.pid).collect(),
                leader: leader.pid,
                rss_kb,
                cpu_pct,
                state,
                tty: members.iter().any(|m| m.tty),
                has_audio: members.iter().any(|m| m.has_audio),
                cmdline: leader.cmdline.clone(),
                argv: leader.argv.clone(),
                age_secs: leader.age_secs,
                kernel: false,
            }
        })
        .collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.rss_kb));
    apps
}

fn pretty_name(key: &str) -> String {
    let mut c = key.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => key.to_string(),
    }
}

// ---------- memory ----------

#[derive(Debug, Clone, Default)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

pub fn meminfo() -> MemInfo {
    let mut m = MemInfo::default();
    if let Some(s) = read_to_string(Path::new("/proc/meminfo")) {
        for line in s.lines() {
            let mut it = line.split_whitespace();
            let (Some(k), Some(v)) = (it.next(), it.next()) else {
                continue;
            };
            let v: u64 = v.parse().unwrap_or(0);
            match k {
                "MemTotal:" => m.total_kb = v,
                "MemAvailable:" => m.available_kb = v,
                "SwapTotal:" => m.swap_total_kb = v,
                "SwapFree:" => m.swap_free_kb = v,
                _ => {}
            }
        }
    }
    m
}

pub fn loadavg() -> (f64, f64, f64) {
    read_to_string(Path::new("/proc/loadavg"))
        .map(|s| {
            let mut it = s.split_whitespace();
            (
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0, 0.0))
}

pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

// ---------- PSI ----------

#[derive(Debug, Clone, Copy, Default)]
pub struct Psi {
    pub some_avg10: f64,
    pub full_avg10: f64,
}

pub fn psi(name: &str) -> Option<Psi> {
    let s = read_to_string(Path::new(&format!("/proc/pressure/{name}")))?;
    let mut p = Psi::default();
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next()?;
        for kv in parts {
            if let Some(v) = kv.strip_prefix("avg10=") {
                let v: f64 = v.parse().unwrap_or(0.0);
                match kind {
                    "some" => p.some_avg10 = v,
                    "full" => p.full_avg10 = v,
                    _ => {}
                }
            }
        }
    }
    Some(p)
}

// ---------- battery / thermals ----------

#[derive(Debug, Clone)]
pub struct Battery {
    pub capacity: u32,
    pub discharging: bool,
}

pub fn battery() -> Option<Battery> {
    let rd = fs::read_dir("/sys/class/power_supply").ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if read_to_string(&p.join("type"))
            .map(|t| t.trim() == "Battery")
            .unwrap_or(false)
        {
            let capacity = read_to_string(&p.join("capacity"))?.trim().parse().ok()?;
            let status = read_to_string(&p.join("status")).unwrap_or_default();
            return Some(Battery {
                capacity,
                discharging: status.trim() == "Discharging",
            });
        }
    }
    None
}

/// Hottest thermal zone in °C, if readable.
pub fn hottest_thermal() -> Option<f64> {
    let rd = fs::read_dir("/sys/class/thermal").ok()?;
    let mut max: Option<f64> = None;
    for e in rd.flatten() {
        if let Some(t) = read_to_string(&e.path().join("temp")) {
            if let Ok(millic) = t.trim().parse::<f64>() {
                let c = millic / 1000.0;
                if c > 0.0 && c < 150.0 {
                    max = Some(max.map_or(c, |m: f64| m.max(c)));
                }
            }
        }
    }
    max
}
