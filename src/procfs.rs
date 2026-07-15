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
        let out = std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok());
        clk_tck_from_output(out.as_deref())
    })
}

/// Parse `getconf CLK_TCK` output; fall back to 100.0 on anything unusable.
fn clk_tck_from_output(out: Option<&str>) -> f64 {
    out.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|&v| v > 0.0)
        .unwrap_or(100.0)
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
        .and_then(|s| parse_uptime(&s))
        .unwrap_or(0.0)
}

/// First field of /proc/uptime: seconds since boot.
fn parse_uptime(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}

/// Fields parsed from /proc/<pid>/stat (after the pid column).
struct StatFields {
    comm: String,
    state: char,
    ppid: u32,
    pgrp: u32,
    tty_nr: i32,
    utime: u64,
    stime: u64,
    starttime: u64,
}

/// Parse /proc/<pid>/stat; comm may contain spaces/parens — parse from the LAST ')'.
fn parse_stat(stat: &str) -> Option<StatFields> {
    let close = stat.rfind(')')?;
    let comm = stat[stat.find('(')? + 1..close].to_string();
    let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
    if f.len() < 22 {
        return None;
    }
    // f[0] is field 3 (state)
    Some(StatFields {
        comm,
        state: f[0].chars().next().unwrap_or('?'),
        ppid: f[1].parse().ok()?,
        pgrp: f[2].parse().ok()?,
        tty_nr: f[4].parse().unwrap_or(0),
        utime: f[11].parse().unwrap_or(0),
        stime: f[12].parse().unwrap_or(0),
        starttime: f[19].parse().unwrap_or(0),
    })
}

/// Split a raw /proc/<pid>/cmdline buffer (NUL-separated) into argv.
fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Pull (uid, rss_kb, threads) out of /proc/<pid>/status.
/// None only when a known key line has no value at all.
fn parse_status(status: &str) -> Option<(u32, u64, u32)> {
    let mut uid = 0u32;
    let mut rss_kb = 0u64;
    let mut threads = 1u32;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            uid = v.split_whitespace().next()?.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss_kb = v.split_whitespace().next()?.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Threads:") {
            threads = v.trim().parse().unwrap_or(1);
        }
    }
    Some((uid, rss_kb, threads))
}

pub fn read_process(pid: u32, uptime: f64) -> Option<Process> {
    let stat = read_to_string(Path::new(&format!("/proc/{pid}/stat")))?;
    let st = parse_stat(&stat)?;

    let cmdline_raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let kernel_thread = cmdline_raw.is_empty();
    let argv = parse_cmdline(&cmdline_raw);
    let cmdline = argv.join(" ");

    let mut uid = 0u32;
    let mut rss_kb = 0u64;
    let mut threads = 1u32;
    if let Some(status) = read_to_string(Path::new(&format!("/proc/{pid}/status"))) {
        let (u, r, t) = parse_status(&status)?;
        uid = u;
        rss_kb = r;
        threads = t;
    }

    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|n| n.trim_end_matches(" (deleted)").to_string())
        .unwrap_or_else(|| st.comm.clone());

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
        ppid: st.ppid,
        pgrp: st.pgrp,
        state: st.state,
        tty: st.tty_nr != 0,
        comm: st.comm,
        exe,
        cmdline,
        argv,
        uid,
        rss_kb,
        threads,
        utime: st.utime,
        stime: st.stime,
        age_secs: (uptime - st.starttime as f64 / ticks_per_sec()).max(0.0),
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
    read_to_string(Path::new("/proc/meminfo"))
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default()
}

fn parse_meminfo(s: &str) -> MemInfo {
    let mut m = MemInfo::default();
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
    m
}

pub fn loadavg() -> (f64, f64, f64) {
    read_to_string(Path::new("/proc/loadavg"))
        .map(|s| parse_loadavg(&s))
        .unwrap_or((0.0, 0.0, 0.0))
}

fn parse_loadavg(s: &str) -> (f64, f64, f64) {
    let mut it = s.split_whitespace();
    (
        it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
        it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
        it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
    )
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
    parse_psi(&s)
}

/// Parse a /proc/pressure/<name> file; None if any line is blank.
fn parse_psi(s: &str) -> Option<Psi> {
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

#[cfg(test)]
mod tests {
    use super::*;

    // Fields 3..24 of a plausible /proc/<pid>/stat, after "<pid> (<comm>) ".
    const STAT_REST: &str = "S 1233 1234 1234 34816 4567 4194304 1234 5678 \
                             0 0 20 10 0 0 20 0 1 0 987654 123456789 1500";

    fn stat_line(pid: u32, comm: &str, rest: &str) -> String {
        format!("{pid} ({comm}) {rest}")
    }

    fn proc(pid: u32, exe: &str, rss_kb: u64, state: char, age_secs: f64) -> Process {
        Process {
            pid,
            ppid: 1,
            pgrp: pid,
            state,
            tty: false,
            comm: exe.to_string(),
            exe: exe.to_string(),
            cmdline: format!("{exe} --flag"),
            argv: vec![exe.to_string(), "--flag".to_string()],
            uid: 1000,
            rss_kb,
            threads: 1,
            utime: 0,
            stime: 0,
            age_secs,
            has_audio: false,
            kernel_thread: false,
        }
    }

    // ---------- /proc/<pid>/stat ----------

    #[test]
    fn parses_typical_stat() {
        let st = parse_stat(&stat_line(1234, "bash", STAT_REST)).expect("stat");
        assert_eq!(st.comm, "bash");
        assert_eq!(st.state, 'S');
        assert_eq!(st.ppid, 1233);
        assert_eq!(st.pgrp, 1234);
        assert_eq!(st.tty_nr, 34816);
        assert_eq!(st.utime, 20);
        assert_eq!(st.stime, 10);
        assert_eq!(st.starttime, 987654);
    }

    #[test]
    fn parses_stat_comm_with_spaces_and_parens() {
        let st = parse_stat(&stat_line(4321, "Web Content (GPU)", STAT_REST)).expect("stat");
        assert_eq!(st.comm, "Web Content (GPU)");
        assert_eq!(st.ppid, 1233);
        // comm ending in ')' — split happens at the LAST ')'
        let st = parse_stat(&stat_line(7, "foo)bar", STAT_REST)).expect("stat");
        assert_eq!(st.comm, "foo)bar");
    }

    #[test]
    fn rejects_stat_without_parens() {
        assert!(parse_stat("1234 bash S 1 2 3").is_none());
        assert!(parse_stat(")").is_none());
    }

    #[test]
    fn rejects_truncated_stat() {
        // fewer than 22 fields after comm
        assert!(parse_stat("1234 (bash) S 1 2 3 4 5").is_none());
        assert!(parse_stat("1234 (bash) ").is_none());
    }

    #[test]
    fn rejects_stat_with_nonnumeric_ppid() {
        let bad = STAT_REST.replacen("1233", "xyz", 1);
        assert!(parse_stat(&stat_line(1, "bash", &bad)).is_none());
    }

    // ---------- /proc/<pid>/cmdline ----------

    #[test]
    fn splits_nul_separated_cmdline() {
        let argv = parse_cmdline(b"firefox\0--new-window\0about:blank\0");
        assert_eq!(argv, vec!["firefox", "--new-window", "about:blank"]);
    }

    #[test]
    fn cmdline_drops_empty_entries() {
        assert!(parse_cmdline(b"").is_empty());
        assert!(parse_cmdline(b"\0\0").is_empty());
        assert_eq!(parse_cmdline(b"a\0\0b\0"), vec!["a", "b"]);
    }

    #[test]
    fn cmdline_is_lossy_on_invalid_utf8() {
        assert_eq!(parse_cmdline(b"a\xffb\0"), vec!["a\u{fffd}b"]);
    }

    // ---------- /proc/<pid>/status ----------

    #[test]
    fn parses_status_uid_rss_threads() {
        let status = "Name:\tfirefox\nUid:\t1000\t1000\t1000\t1000\n\
                      Gid:\t1000\t1000\t1000\t1000\nVmRSS:\t  245812 kB\nThreads:\t7\n";
        assert_eq!(parse_status(status), Some((1000, 245812, 7)));
    }

    #[test]
    fn status_defaults_when_keys_missing() {
        assert_eq!(parse_status("Name:\tkthreadd\n"), Some((0, 0, 1)));
        assert_eq!(parse_status(""), Some((0, 0, 1)));
    }

    #[test]
    fn status_rejects_key_without_value() {
        assert!(parse_status("Uid:\n").is_none());
        assert!(parse_status("VmRSS:\n").is_none());
    }

    // ---------- /proc/uptime ----------

    #[test]
    fn parses_uptime_first_field() {
        assert_eq!(parse_uptime("12345.67 89012.34\n"), Some(12345.67));
        assert!(parse_uptime("garbage").is_none());
        assert!(parse_uptime("").is_none());
    }

    // ---------- getconf CLK_TCK fallback ----------

    #[test]
    fn parses_valid_clk_tck() {
        assert_eq!(clk_tck_from_output(Some("100\n")), 100.0);
        assert_eq!(clk_tck_from_output(Some(" 250 ")), 250.0);
    }

    #[test]
    fn clk_tck_falls_back_to_100() {
        assert_eq!(clk_tck_from_output(None), 100.0);
        assert_eq!(clk_tck_from_output(Some("garbage")), 100.0);
        assert_eq!(clk_tck_from_output(Some("")), 100.0);
        assert_eq!(clk_tck_from_output(Some("0")), 100.0);
        assert_eq!(clk_tck_from_output(Some("-5")), 100.0);
    }

    // ---------- /proc/meminfo ----------

    #[test]
    fn parses_meminfo_fields() {
        let s = "MemTotal:       16384000 kB\nMemFree:         1234567 kB\n\
                 MemAvailable:    8000000 kB\nBuffers:          100000 kB\n\
                 SwapTotal:       2097152 kB\nSwapFree:        1048576 kB\n";
        let m = parse_meminfo(s);
        assert_eq!(m.total_kb, 16384000);
        assert_eq!(m.available_kb, 8000000);
        assert_eq!(m.swap_total_kb, 2097152);
        assert_eq!(m.swap_free_kb, 1048576);
    }

    #[test]
    fn meminfo_ignores_unknown_and_malformed_lines() {
        let s = "garbage line\nHugePages_Total: 0\nMemTotal: notanumber kB\n";
        let m = parse_meminfo(s);
        assert_eq!(m.total_kb, 0);
        assert_eq!(m.available_kb, 0);
        assert!(parse_meminfo("").total_kb == 0);
    }

    // ---------- /proc/loadavg ----------

    #[test]
    fn parses_loadavg_triple() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 2/1234 5678\n"),
            (0.52, 0.58, 0.59)
        );
    }

    #[test]
    fn short_or_garbage_loadavg_yields_zeros() {
        assert_eq!(parse_loadavg(""), (0.0, 0.0, 0.0));
        assert_eq!(parse_loadavg("1.5"), (1.5, 0.0, 0.0));
        assert_eq!(parse_loadavg("x y z"), (0.0, 0.0, 0.0));
    }

    // ---------- PSI ----------

    #[test]
    fn parses_psi_some_and_full() {
        let s = "some avg10=1.50 avg60=0.02 avg300=0.01 total=12345\n\
                 full avg10=0.25 avg60=0.01 avg300=0.00 total=6789\n";
        let p = parse_psi(s).expect("psi");
        assert_eq!(p.some_avg10, 1.50);
        assert_eq!(p.full_avg10, 0.25);
    }

    #[test]
    fn psi_without_full_line_leaves_zero() {
        // /proc/pressure/cpu has no "full" line on some kernels
        let p = parse_psi("some avg10=2.00 avg60=0.00 avg300=0.00 total=1\n").expect("psi");
        assert_eq!(p.some_avg10, 2.00);
        assert_eq!(p.full_avg10, 0.0);
    }

    #[test]
    fn psi_ignores_unknown_kinds() {
        let p = parse_psi("weird avg10=9.99\nsome avg10=1.00\n").expect("psi");
        assert_eq!(p.some_avg10, 1.00);
        assert_eq!(p.full_avg10, 0.0);
    }

    #[test]
    fn psi_rejects_blank_lines() {
        assert!(parse_psi("some avg10=1.0\n\n").is_none());
        assert!(parse_psi("\n").is_none());
    }

    // ---------- app grouping ----------

    #[test]
    fn groups_by_lowercased_exe_and_sums_rss() {
        let procs = vec![
            proc(100, "Firefox", 1000, 'S', 50.0),
            proc(101, "firefox", 2000, 'S', 10.0),
        ];
        let apps = group_apps(procs, 1000);
        assert_eq!(apps.len(), 1);
        let app = &apps[0];
        assert_eq!(app.key, "firefox");
        assert_eq!(app.display, "Firefox");
        assert_eq!(app.pids, vec![100, 101]);
        assert_eq!(app.rss_kb, 3000);
        assert!(!app.kernel);
    }

    #[test]
    fn group_excludes_kernel_threads_and_pid1() {
        let mut kthread = proc(50, "kworker", 0, 'S', 99.0);
        kthread.kernel_thread = true;
        let init = proc(1, "systemd", 500, 'S', 9999.0);
        let real = proc(60, "bash", 100, 'S', 5.0);
        let apps = group_apps(vec![kthread, init, real], 1000);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].key, "bash");
    }

    #[test]
    fn leader_is_oldest_member() {
        let procs = vec![
            proc(5, "bash", 100, 'S', 100.0),
            proc(6, "bash", 100, 'S', 50.0),
        ];
        let apps = group_apps(procs, 1000);
        assert_eq!(apps[0].leader, 5);
        assert_eq!(apps[0].age_secs, 100.0);
        // representative cmdline/argv come from the leader
        assert_eq!(apps[0].argv, vec!["bash", "--flag"]);
    }

    #[test]
    fn group_state_prefers_running_then_sleeping() {
        let rank = |states: &[char]| {
            let procs: Vec<Process> = states
                .iter()
                .enumerate()
                .map(|(i, &s)| proc(10 + i as u32, "x", 1, s, i as f64))
                .collect();
            group_apps(procs, 1000)[0].state
        };
        assert_eq!(rank(&['T', 'R', 'S']), 'R');
        assert_eq!(rank(&['T', 'S']), 'S');
        assert_eq!(rank(&['D', 'T']), 'T');
        assert_eq!(rank(&['Z', 'D']), 'D');
        assert_eq!(rank(&['Z']), 'Z');
    }

    #[test]
    fn group_aggregates_tty_and_audio() {
        let mut a = proc(2, "mpv", 100, 'S', 5.0);
        a.tty = true;
        let mut b = proc(3, "mpv", 100, 'S', 4.0);
        b.has_audio = true;
        let apps = group_apps(vec![a, b], 1000);
        assert!(apps[0].tty);
        assert!(apps[0].has_audio);
    }

    #[test]
    fn apps_sorted_by_rss_descending() {
        let procs = vec![
            proc(10, "small", 100, 'S', 1.0),
            proc(11, "big", 9000, 'S', 1.0),
            proc(12, "mid", 500, 'S', 1.0),
        ];
        let apps = group_apps(procs, 1000);
        let keys: Vec<&str> = apps.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["big", "mid", "small"]);
    }

    #[test]
    fn group_cpu_pct_uses_sampled_ticks() {
        let mut p = proc(10, "burn", 100, 'R', 1.0);
        p.utime = 250; // delta ticks stashed by snapshot()
        p.stime = 0;
        let apps = group_apps(vec![p], 1000); // secs = 1.0
        let expected = 250.0 / ticks_per_sec() * 100.0;
        assert!((apps[0].cpu_pct - expected).abs() < 1e-6);
    }

    #[test]
    fn pretty_name_capitalizes() {
        assert_eq!(pretty_name("firefox"), "Firefox");
        assert_eq!(pretty_name("z"), "Z");
        assert_eq!(pretty_name(""), "");
    }

    #[test]
    fn cpu_ticks_sums_utime_and_stime() {
        let mut p = proc(1, "x", 0, 'S', 0.0);
        p.utime = 11;
        p.stime = 22;
        assert_eq!(p.cpu_ticks(), 33);
    }

    // ---------- xdg fallback ----------

    #[test]
    fn xdg_unset_var_falls_back_to_home_suffix() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert_eq!(
            xdg("PV_PROCFS_TEST_DEFINITELY_UNSET", ".config"),
            std::path::PathBuf::from(home).join(".config")
        );
    }

    #[test]
    fn xdg_empty_var_falls_back_too() {
        // unique dummy var name; never an XDG var, so nothing real is redirected
        let var = format!("PV_PROCFS_TEST_EMPTY_{}", std::process::id());
        std::env::set_var(&var, "");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let got = xdg(&var, ".local/share");
        std::env::remove_var(&var);
        assert_eq!(got, std::path::PathBuf::from(home).join(".local/share"));
    }

    #[test]
    fn xdg_set_var_is_used() {
        let var = format!("PV_PROCFS_TEST_SET_{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("pv-procfs-test-{}", std::process::id()));
        std::env::set_var(&var, &dir);
        let got = xdg(&var, ".config");
        std::env::remove_var(&var);
        assert_eq!(got, dir);
    }
}
