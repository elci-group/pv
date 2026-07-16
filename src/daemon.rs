//! Daemon mode: continuous observation, habit learning, and valve rules.
//!
//! The one-shot commands answer "what is happening?" — the daemon answers
//! "what is happening, what usually happens, and what is about to happen?"

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::display::Theme;
use crate::intent::Intent;
use crate::notify::{self, Level, Notice};
use crate::pressure::{fmt_kb, PressureReport};
use crate::procfs::{self, App};
use crate::recommend::{self, Action};

// ---------------------------------------------------------------- history

const WINDOW: usize = 120; // samples kept (10 min at 5s cadence)

#[derive(Default)]
struct History {
    mem_used_kb: VecDeque<u64>,
    swap_used_kb: VecDeque<u64>,
    load1: VecDeque<f64>,
}

impl History {
    /// Record one sample. `load1` is passed in so the window math stays pure
    /// and testable; the loop reads `/proc/loadavg` at the call site.
    fn push(&mut self, r: &PressureReport, load1: f64) {
        self.mem_used_kb
            .push_back(r.mem.total_kb - r.mem.available_kb);
        self.swap_used_kb
            .push_back(r.mem.swap_total_kb - r.mem.swap_free_kb);
        self.load1.push_back(load1);
        for q in [&mut self.mem_used_kb, &mut self.swap_used_kb] {
            while q.len() > WINDOW {
                q.pop_front();
            }
        }
        while self.load1.len() > WINDOW {
            self.load1.pop_front();
        }
    }

    fn len(&self) -> usize {
        self.mem_used_kb.len()
    }

    /// Slope of memory used in kB/s over the window (positive = growing).
    fn mem_slope_kb_s(&self, interval_secs: u64) -> f64 {
        let n = self.mem_used_kb.len();
        // interval_secs == 0 would divide by zero — report no trend instead
        if n < 2 || interval_secs == 0 {
            return 0.0;
        }
        let first = self.mem_used_kb.front().copied().unwrap_or(0) as f64;
        let last = self.mem_used_kb.back().copied().unwrap_or(0) as f64;
        (last - first) / ((n - 1) as f64 * interval_secs as f64)
    }

    fn swap_growth_kb(&self) -> i64 {
        let n = self.swap_used_kb.len();
        if n < 2 {
            return 0;
        }
        self.swap_used_kb.back().copied().unwrap_or(0) as i64
            - self.swap_used_kb.front().copied().unwrap_or(0) as i64
    }

    /// (mean, coefficient of variation) of load1 over the window.
    fn load_stats(&self) -> (f64, f64) {
        let n = self.load1.len();
        if n < 2 {
            return (0.0, 0.0);
        }
        let mean = self.load1.iter().sum::<f64>() / n as f64;
        if mean <= 0.01 {
            return (mean, 0.0);
        }
        let var = self.load1.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        (mean, var.sqrt() / mean)
    }
}

// ---------------------------------------------------------------- habits

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HabitSlot {
    samples: u64,
    cpu_ema: f64,            // load1/cores %
    mem_ema: f64,            // mem used %
    var_ema: f64,            // load coefficient of variation
    top: Vec<(String, u32)>, // most-seen app categories
}

impl Default for HabitSlot {
    fn default() -> Self {
        HabitSlot {
            samples: 0,
            cpu_ema: 0.0,
            mem_ema: 0.0,
            var_ema: 0.0,
            top: Vec::new(),
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Habits {
    slots: Vec<HabitSlot>, // 24, indexed by local hour
    #[serde(skip)]
    last_save: u64,
}

fn habits_path() -> PathBuf {
    crate::procfs::xdg("XDG_DATA_HOME", ".local/share").join("pv/habits.toml")
}

impl Habits {
    pub fn load() -> Self {
        let mut h: Habits = fs::read_to_string(habits_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or(Habits {
                slots: (0..24).map(|_| HabitSlot::default()).collect(),
                last_save: 0,
            });
        h.slots.resize_with(24, HabitSlot::default);
        h
    }

    pub fn save(&mut self) {
        if let Some(parent) = habits_path().parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(habits_path(), toml::to_string_pretty(self).unwrap());
        self.last_save = now_secs();
    }

    fn observe(&mut self, cpu_pct: f64, mem_pct: f64, cv: f64, categories: &[String]) {
        self.observe_at(notify::local_hour(), cpu_pct, mem_pct, cv, categories);
    }

    /// Record one sample into the slot for `hour`. Split from `observe` so the
    /// accumulation logic can be tested without the wall clock.
    fn observe_at(
        &mut self,
        hour: usize,
        cpu_pct: f64,
        mem_pct: f64,
        cv: f64,
        categories: &[String],
    ) {
        let s = &mut self.slots[hour % 24];
        let a = if s.samples < 50 { 0.2 } else { 0.05 }; // learn fast early
        s.cpu_ema = if s.samples == 0 {
            cpu_pct
        } else {
            s.cpu_ema * (1.0 - a) + cpu_pct * a
        };
        s.mem_ema = if s.samples == 0 {
            mem_pct
        } else {
            s.mem_ema * (1.0 - a) + mem_pct * a
        };
        s.var_ema = if s.samples == 0 {
            cv
        } else {
            s.var_ema * (1.0 - a) + cv * a
        };
        for c in categories {
            if let Some(e) = s.top.iter_mut().find(|(k, _)| k == c) {
                e.1 += 1;
            } else {
                s.top.push((c.clone(), 1));
            }
        }
        s.top.sort_by_key(|x| std::cmp::Reverse(x.1));
        s.top.truncate(3);
        s.samples += 1;
    }

    fn slot(&self, hour: usize) -> &HabitSlot {
        &self.slots[hour % 24]
    }

    pub fn total_samples(&self) -> u64 {
        self.slots.iter().map(|s| s.samples).sum()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- valves

struct Ctx<'a> {
    hist: &'a History,
    habits: &'a Habits,
    apps: &'a [App],
    intents: &'a [(String, Intent)],
    report: &'a PressureReport,
    interval: u64,
}

fn mem_used_pct(r: &PressureReport) -> f64 {
    (1.0 - r.mem.available_kb as f64 / r.mem.total_kb.max(1) as f64) * 100.0
}

/// Ambient system readings the valves consult, sampled once per daemon tick
/// so the decision step below is a pure function of observed state + habits.
#[derive(Debug, Clone)]
struct SysSample {
    ncpu: usize,
    load1: f64,
    psi_mem_some_avg10: f64,
    hottest_thermal: Option<f64>,
    hour: usize,
}

impl SysSample {
    fn read() -> Self {
        SysSample {
            ncpu: procfs::cpu_count(),
            load1: procfs::loadavg().0,
            psi_mem_some_avg10: procfs::psi("memory").unwrap_or_default().some_avg10,
            hottest_thermal: procfs::hottest_thermal(),
            hour: notify::local_hour(),
        }
    }
}

/// Valves that only need the current moment — shared by `pv notify`.
pub fn oneshot_notices(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
) -> Vec<Notice> {
    oneshot_notices_at(apps, intents, report, procfs::hottest_thermal())
}

/// Same valves with the thermal reading passed in — the pure core.
fn oneshot_notices_at(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
    hottest_thermal: Option<f64>,
) -> Vec<Notice> {
    let mut out = Vec::new();
    let used = mem_used_pct(report);
    let recs = recommend::recommend_with_labels(apps, intents, report, &crate::label::load());

    // V-01 memory pressure, current reading
    if used >= 85.0 || report.oom_eta_secs.map(|e| e < 600).unwrap_or(false) {
        let reclaim = recs
            .iter()
            .find(|r| r.action == Action::Suspend)
            .map(|r| r.target.clone());
        out.push(Notice {
            level: Level::Critical,
            valve: "V-01",
            title: "MEMORY OVERPRESSURE".into(),
            gauge: Some(("RAM".into(), used.round() as u8)),
            obs: {
                let mut v = vec![format!("{:.0}% of physical memory committed", used)];
                if let Some(eta) = report.oom_eta_secs {
                    v.push(format!(
                        "projected exhaustion in {}",
                        crate::pressure::fmt_eta(eta)
                    ));
                }
                if let Some(t) = &reclaim {
                    v.push(format!("largest reclaimable reserve: {t}"));
                }
                v
            },
            suggest: reclaim.map(|t| format!("pv suspend {t}")),
        });
    }

    // V-03 reclaimable idle app
    if let Some(r) = recs.iter().find(|r| r.action == Action::Suspend) {
        out.push(Notice {
            level: Level::Advisory,
            valve: "V-03",
            title: format!("RECLAIMABLE RESERVE: {}", r.target.to_uppercase()),
            gauge: Some(("RAM".into(), report.memory.score)),
            obs: vec![
                format!(
                    "{} sits idle — {} recoverable",
                    r.target,
                    fmt_kb(r.benefit_kb)
                ),
                format!("confidence {}%", r.confidence),
            ],
            suggest: Some(format!("pv suspend {}", r.target)),
        });
    }

    // V-07 battery
    if let Some(b) = &report.battery_info {
        if b.discharging && b.capacity <= 15 {
            let builds: Vec<&str> = apps
                .iter()
                .filter(|a| {
                    intents
                        .iter()
                        .find(|(k, _)| k == &a.key)
                        .map(|(_, i)| i.can_migrate)
                        .unwrap_or(false)
                })
                .map(|a| a.display.as_str())
                .collect();
            out.push(Notice {
                level: if b.capacity <= 8 {
                    Level::Critical
                } else {
                    Level::Warning
                },
                valve: "V-07",
                title: "BATTERY VENTING".into(),
                gauge: Some(("BAT".into(), 100u8.saturating_sub(b.capacity as u8))),
                obs: vec![
                    format!("{}% remaining, discharging", b.capacity),
                    if builds.is_empty() {
                        "no migratable workloads running".into()
                    } else {
                        format!("migratable workloads: {}", builds.join(", "))
                    },
                ],
                suggest: builds
                    .first()
                    .map(|b| format!("pv migrate {}", b.to_lowercase())),
            });
        }
    }

    // V-08 thermals
    if let Some(temp) = hottest_thermal {
        if temp >= 82.0 {
            let hog = apps
                .iter()
                .filter(|a| {
                    intents
                        .iter()
                        .find(|(k, _)| k == &a.key)
                        .map(|(_, i)| !i.never_suspend)
                        .unwrap_or(true)
                })
                .max_by(|a, b| a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap())
                .map(|a| a.display.clone());
            out.push(Notice {
                level: if temp >= 90.0 {
                    Level::Critical
                } else {
                    Level::Warning
                },
                valve: "V-08",
                title: "THERMAL OVERPRESSURE".into(),
                gauge: Some((
                    "TEMP".into(),
                    ((temp - 40.0) / 60.0 * 100.0).clamp(0.0, 100.0) as u8,
                )),
                obs: vec![
                    format!("{temp:.0}°C hottest zone"),
                    hog.as_ref()
                        .map(|h| format!("largest heat source: {h}"))
                        .unwrap_or_default(),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect(),
                suggest: hog.map(|h| format!("pv suspend {}", h.to_lowercase())),
            });
        }
    }
    out
}

/// Valves that need history and habit context — daemon only.
/// Pure: every ambient reading arrives via `sys`.
fn history_notices(ctx: &Ctx, sys: &SysSample) -> Vec<Notice> {
    let mut out = Vec::new();
    let r = ctx.report;
    let used = mem_used_pct(r);
    let ncpu = sys.ncpu as f64;
    let cpu_pct = sys.load1 / ncpu * 100.0;
    let slope = ctx.hist.mem_slope_kb_s(ctx.interval);

    // V-01 escalation: sustained climb before we hit the critical line
    if (72.0..85.0).contains(&used) && slope > 2048.0 && ctx.hist.len() >= 12 {
        out.push(Notice {
            level: Level::Warning,
            valve: "V-01",
            title: "MEMORY PRESSURE RISING".into(),
            gauge: Some(("RAM".into(), used as u8)),
            obs: vec![
                format!(
                    "{:.0}% committed and climbing +{:.1} MB/s",
                    used,
                    slope / 1024.0
                ),
                "trend sustained across the observation window".into(),
            ],
            suggest: ctx
                .apps
                .iter()
                .find(|a| {
                    ctx.intents
                        .iter()
                        .find(|(k, _)| k == &a.key)
                        .map(|(_, i)| i.can_suspend && !i.never_suspend)
                        .unwrap_or(false)
                })
                .map(|a| format!("pv suspend {}", a.key)),
        });
    }

    // V-02 demand spike vs habit baseline for this hour
    let slot = ctx.habits.slot(sys.hour);
    if slot.samples >= 20 && slot.cpu_ema > 5.0 && cpu_pct > slot.cpu_ema * 1.8 && cpu_pct > 40.0 {
        let hog = ctx
            .apps
            .iter()
            .max_by(|a, b| a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap())
            .map(|a| a.display.clone());
        out.push(Notice {
            level: Level::Advisory,
            valve: "V-02",
            title: "UNUSUAL DEMAND FOR THIS HOUR".into(),
            gauge: Some(("CPU".into(), cpu_pct.min(100.0) as u8)),
            obs: vec![
                format!(
                    "load {:.0}% — your usual here is {:.0}%",
                    cpu_pct, slot.cpu_ema
                ),
                hog.map(|h| format!("primary source: {h}"))
                    .unwrap_or_default(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
            suggest: None,
        });
    }

    // V-04 swap thrash risk
    let psi_some = sys.psi_mem_some_avg10;
    if ctx.hist.swap_growth_kb() > 50_000 && psi_some > 1.0 {
        out.push(Notice {
            level: Level::Warning,
            valve: "V-04",
            title: "SWAP THRASH RISK".into(),
            gauge: Some(("MEM-PSI".into(), psi_some.min(100.0) as u8)),
            obs: vec![
                format!(
                    "swap grew {} across the window",
                    fmt_kb(ctx.hist.swap_growth_kb() as u64)
                ),
                format!(
                    "memory stall {:.0}% — the kernel is paging under load",
                    psi_some
                ),
            ],
            suggest: Some("relieve pressure or expect stalls".into()),
        });
    }

    // V-05 erratic demand
    let (mean_load, cv) = ctx.hist.load_stats();
    if ctx.hist.len() >= 24 && cv > 0.55 && mean_load > 1.0 {
        out.push(Notice {
            level: Level::Advisory,
            valve: "V-05",
            title: "ERRATIC DEMAND PATTERN".into(),
            gauge: Some(("VAR".into(), (cv * 100.0).min(100.0) as u8)),
            obs: vec![
                format!(
                    "load swinging ±{:.0}% around mean {:.1}",
                    cv * 100.0,
                    mean_load
                ),
                "bursty workloads thrash caches and scheduler".into(),
            ],
            suggest: Some("stagger heavy jobs instead of firing them together".into()),
        });
    }

    // V-06 habit forecast: next hour historically heavier
    let next = ctx.habits.slot(sys.hour + 1);
    if next.samples >= 20 && next.mem_ema - used > 15.0 {
        out.push(Notice {
            level: Level::Advisory,
            valve: "V-06",
            title: "DEMAND FRONT INCOMING".into(),
            gauge: Some(("RAM".into(), next.mem_ema as u8)),
            obs: vec![
                format!(
                    "your {}:00 block usually runs ~{:.0}% RAM (now {:.0}%)",
                    (sys.hour + 1) % 24,
                    next.mem_ema,
                    used
                ),
                if next.top.is_empty() {
                    String::new()
                } else {
                    format!(
                        "usual suspects: {}",
                        next.top
                            .iter()
                            .map(|(c, _)| c.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
            suggest: Some("run heavy jobs now, or plan a migration".into()),
        });
    }
    out
}

/// The daemon's decision step as one pure function: observed system state +
/// learned habits in, planned notices out. The loop gathers the inputs;
/// tests drive this directly.
fn plan_notices(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
    hist: &History,
    habits: &Habits,
    interval: u64,
    sys: &SysSample,
) -> Vec<Notice> {
    let mut notices = oneshot_notices_at(apps, intents, report, sys.hottest_thermal);
    notices.extend(history_notices(
        &Ctx {
            hist,
            habits,
            apps,
            intents,
            report,
            interval,
        },
        sys,
    ));
    notices
}

// ---------------------------------------------------------------- loop

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    QUIT.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

pub fn run_daemon(t: &Theme, interval: u64, desktop: bool) -> i32 {
    unsafe {
        let handler = on_signal as extern "C" fn(i32) as usize;
        signal(2, handler); // SIGINT
        signal(15, handler); // SIGTERM
    }
    let mut hist = History::default();
    let mut habits = Habits::load();
    let mut cooldowns = notify::Cooldowns::new();

    println!(
        "{}",
        t.dim(&format!(
            "┌─[ PV-DAEMON ]── online · 8 valves armed · interval {interval}s · habits: {} samples",
            habits.total_samples()
        ))
    );
    println!("{}", t.dim("└─ watching… (ctrl-c to stop)"));

    while !QUIT.load(Ordering::SeqCst) {
        let procs = procfs::snapshot(150);
        let apps = procfs::group_apps(procs, 150);
        let intents: Vec<(String, Intent)> = apps
            .iter()
            .map(|a| (a.key.clone(), crate::intent::classify(a)))
            .collect();
        let report = crate::pressure::measure(250);

        hist.push(&report, procfs::loadavg().0);
        let ncpu = procfs::cpu_count() as f64;
        let cpu_pct = procfs::loadavg().0 / ncpu * 100.0;
        let (_, cv) = hist.load_stats();
        let cats: Vec<String> = intents
            .iter()
            .filter(|(k, _)| {
                apps.iter()
                    .find(|a| &a.key == k)
                    .map(|a| a.cpu_pct > 5.0 || a.rss_kb > 200_000)
                    .unwrap_or(false)
            })
            .map(|(_, i)| format!("{:?}", i.category).to_lowercase())
            .collect();
        habits.observe(cpu_pct, mem_used_pct(&report), cv, &cats);
        if now_secs().saturating_sub(habits.last_save) > 60 {
            habits.save();
        }

        let sys = SysSample::read();
        let notices = plan_notices(&apps, &intents, &report, &hist, &habits, interval, &sys);

        for n in notices {
            let key = format!("{}:{}", n.valve, n.title);
            if cooldowns.allow(&key, n.level) {
                print!("{}", notify::render(t, &n));
                if desktop {
                    notify::desktop(&n);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
    habits.save();
    println!("{}", t.dim("└─ pv-daemon: habits saved, valves closed"));
    0
}

// ---------------------------------------------------------------- habits view

pub fn print_habits(t: &Theme) -> i32 {
    let h = Habits::load();
    if h.total_samples() == 0 {
        println!(
            "{}",
            t.dim("No habit data yet — run `pv daemon` for a while and it will learn your rhythm.")
        );
        return 0;
    }
    println!(
        "{}",
        t.bold("Learned demand profile (per hour, local time)")
    );
    let cur = notify::local_hour();
    for (hour, s) in h.slots.iter().enumerate() {
        if s.samples == 0 {
            continue;
        }
        let marker = if hour == cur {
            t.cyan("→")
        } else {
            " ".to_string()
        };
        let cpu_bar = crate::display::bar((s.cpu_ema as u8).min(100), 8);
        let mem_bar = crate::display::bar((s.mem_ema as u8).min(100), 8);
        let cats = if s.top.is_empty() {
            String::new()
        } else {
            format!(
                "  {}",
                s.top
                    .iter()
                    .map(|(c, _)| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!(
            "  {} {:02}:00  cpu {:>4.0}% {}  mem {:>4.0}% {}  var {:.2}{}",
            marker,
            hour,
            s.cpu_ema,
            t.dim(&cpu_bar),
            s.mem_ema,
            t.dim(&mem_bar),
            s.var_ema,
            t.dim(&cats)
        );
    }
    println!(
        "{}",
        t.dim(&format!(
            "\n{} samples — profile strengthens as the daemon keeps watching.",
            h.total_samples()
        ))
    );
    0
}

/// One-shot `pv notify`: current-state valves only, no cooldowns.
pub fn run_notify(t: &Theme, desktop: bool) -> i32 {
    let procs = procfs::snapshot(150);
    let apps = procfs::group_apps(procs, 150);
    let intents: Vec<(String, Intent)> = apps
        .iter()
        .map(|a| (a.key.clone(), crate::intent::classify(a)))
        .collect();
    let report = crate::pressure::measure(400);
    let notices = oneshot_notices(&apps, &intents, &report);
    if notices.is_empty() {
        let n = Notice {
            level: Level::Advisory,
            valve: "V-00",
            title: "ALL VALVES NOMINAL".into(),
            gauge: Some(("SYS".into(), report.overall)),
            obs: vec![
                format!(
                    "RAM {:.0}% · load {:.2} · no reclaimable reserves",
                    mem_used_pct(&report),
                    procfs::loadavg().0
                ),
                "no action suggested at this time".into(),
            ],
            suggest: None,
        };
        print!("{}", notify::render(t, &n));
        return 0;
    }
    for n in &notices {
        print!("{}", notify::render(t, n));
        if desktop {
            notify::desktop(n);
        }
    }
    0
}

// ---------------------------------------------------------------- install

pub fn install_service() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[pv] cannot resolve own path: {e}");
            return 1;
        }
    };
    let unit = format!(
        "[Unit]\nDescription=Pressure Valve daemon\nAfter=default.target\n\n[Service]\nExecStart={} daemon --desktop\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
        exe.display()
    );
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .join(".config/systemd/user");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[pv] {e}");
        return 1;
    }
    let path = dir.join("pv-daemon.service");
    if let Err(e) = fs::write(&path, unit) {
        eprintln!("[pv] {e}");
        return 1;
    }
    println!("wrote {}", path.display());
    println!("enable with:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now pv-daemon");
    0
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure::ResourcePressure;
    use crate::procfs::{Battery, MemInfo};

    const TOTAL_KB: u64 = 16_000_000; // pretend 16 GB machine

    fn rp(name: &'static str, score: u8) -> ResourcePressure {
        ResourcePressure {
            name,
            score,
            detail: String::new(),
        }
    }

    /// A calm report: low pressure everywhere, nothing draining.
    fn base_report(total_kb: u64, available_kb: u64) -> PressureReport {
        PressureReport {
            cpu: rp("CPU", 10),
            memory: rp("RAM", 20),
            io: rp("IO", 0),
            battery: None,
            thermal: None,
            mem: MemInfo {
                total_kb,
                available_kb,
                swap_total_kb: 0,
                swap_free_kb: 0,
            },
            battery_info: None,
            mem_rate_kb_s: 0.0,
            oom_eta_secs: None,
            overall: 20,
        }
    }

    fn report_with_used_pct(used_pct: f64) -> PressureReport {
        let avail = (TOTAL_KB as f64 * (100.0 - used_pct) / 100.0) as u64;
        base_report(TOTAL_KB, avail)
    }

    fn app(key: &str, display: &str, rss_kb: u64, cpu_pct: f64) -> App {
        App {
            key: key.into(),
            display: display.into(),
            pids: vec![4242],
            leader: 4242,
            rss_kb,
            cpu_pct,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: key.into(),
            argv: vec![key.into()],
            age_secs: 7200.0,
            kernel: false,
        }
    }

    fn intents_of(apps: &[App]) -> Vec<(String, Intent)> {
        apps.iter()
            .map(|a| (a.key.clone(), crate::intent::classify(a)))
            .collect()
    }

    fn fresh_habits() -> Habits {
        Habits {
            slots: (0..24).map(|_| HabitSlot::default()).collect(),
            last_save: 0,
        }
    }

    /// Ambient readings of a healthy machine at 09:00.
    fn quiet_sys() -> SysSample {
        SysSample {
            ncpu: 8,
            load1: 0.4,
            psi_mem_some_avg10: 0.0,
            hottest_thermal: None,
            hour: 9,
        }
    }

    fn valves(notices: &[Notice]) -> Vec<&'static str> {
        notices.iter().map(|n| n.valve).collect()
    }

    // ---------- the decision step ----------

    #[test]
    fn quiet_system_produces_no_notices() {
        let apps = vec![app("code", "Code", 800_000, 0.5)];
        let intents = intents_of(&apps);
        let report = report_with_used_pct(30.0);
        let mut hist = History::default();
        for _ in 0..6 {
            hist.push(&report, 0.4);
        }
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &hist,
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert!(
            notices.is_empty(),
            "quiet system should plan nothing, got {:?}",
            valves(&notices)
        );
    }

    #[test]
    fn rising_memory_trend_warns_before_critical() {
        let mut hist = History::default();
        // 24 samples at 5s cadence climbing 70% -> ~79% committed
        for i in 0..24u64 {
            hist.push(&report_with_used_pct(70.0 + i as f64 * 0.4), 0.5);
        }
        assert!(hist.mem_slope_kb_s(5) > 2048.0);
        assert!(hist.len() >= 12);

        let report = report_with_used_pct(79.0);
        let notices = plan_notices(&[], &[], &report, &hist, &fresh_habits(), 5, &quiet_sys());
        assert_eq!(valves(&notices), vec!["V-01"]);
        let v1 = &notices[0];
        assert_eq!(v1.level, Level::Warning);
        assert_eq!(v1.title, "MEMORY PRESSURE RISING");
        assert!(v1.obs[0].contains("committed and climbing"));
    }

    #[test]
    fn committed_memory_fires_critical_overpressure() {
        let mut report = report_with_used_pct(90.0);
        report.memory.score = 90;
        let notices = plan_notices(
            &[],
            &[],
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert_eq!(valves(&notices), vec!["V-01"]);
        let v1 = &notices[0];
        assert_eq!(v1.level, Level::Critical);
        assert_eq!(v1.title, "MEMORY OVERPRESSURE");
        assert_eq!(v1.gauge, Some(("RAM".to_string(), 90)));
    }

    #[test]
    fn oom_eta_under_ten_minutes_fires_overpressure() {
        // 60% committed is below the 85% line; the ETA alone must trip V-01.
        let mut report = report_with_used_pct(60.0);
        report.oom_eta_secs = Some(300);
        let notices = plan_notices(
            &[],
            &[],
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert_eq!(valves(&notices), vec!["V-01"]);
        let v1 = &notices[0];
        assert_eq!(v1.level, Level::Critical);
        assert!(v1
            .obs
            .iter()
            .any(|o| o.contains("projected exhaustion in 5m 00s")));
    }

    #[test]
    fn idle_browser_surfaces_reclaimable_reserve() {
        let apps = vec![app("firefox", "Firefox", 2_000_000, 0.2)];
        let intents = intents_of(&apps);
        let mut report = report_with_used_pct(60.0);
        report.memory.score = 60; // warm enough to look for reclaim
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert_eq!(valves(&notices), vec!["V-03"]);
        let v3 = &notices[0];
        assert_eq!(v3.level, Level::Advisory);
        assert_eq!(v3.title, "RECLAIMABLE RESERVE: FIREFOX");
        assert_eq!(v3.suggest.as_deref(), Some("pv suspend firefox"));
    }

    #[test]
    fn busy_browser_is_not_reclaimable() {
        let apps = vec![app("firefox", "Firefox", 2_000_000, 35.0)];
        let intents = intents_of(&apps);
        let mut report = report_with_used_pct(60.0);
        report.memory.score = 60;
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert!(notices.iter().all(|n| n.valve != "V-03"));
    }

    #[test]
    fn low_battery_with_migratable_build_suggests_migration() {
        let apps = vec![app("cargo", "Cargo", 500_000, 85.0)];
        let intents = intents_of(&apps);
        let mut report = report_with_used_pct(30.0);
        report.battery_info = Some(Battery {
            capacity: 12,
            discharging: true,
        });
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert_eq!(valves(&notices), vec!["V-07"]);
        let v7 = &notices[0];
        assert_eq!(v7.level, Level::Warning); // 12%: serious, not yet critical
        assert_eq!(v7.gauge, Some(("BAT".to_string(), 88)));
        assert!(v7
            .obs
            .iter()
            .any(|o| o.contains("migratable workloads: Cargo")));
        assert_eq!(v7.suggest.as_deref(), Some("pv migrate cargo"));
    }

    #[test]
    fn critical_battery_without_migratable_work_escalates() {
        let apps = vec![app("firefox", "Firefox", 900_000, 5.0)];
        let intents = intents_of(&apps);
        let mut report = report_with_used_pct(30.0);
        report.battery_info = Some(Battery {
            capacity: 6,
            discharging: true,
        });
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert_eq!(valves(&notices), vec!["V-07"]);
        let v7 = &notices[0];
        assert_eq!(v7.level, Level::Critical); // <= 8% is critical
        assert!(v7
            .obs
            .iter()
            .any(|o| o.contains("no migratable workloads running")));
        assert!(v7.suggest.is_none());
    }

    #[test]
    fn charging_battery_stays_quiet() {
        let mut report = report_with_used_pct(30.0);
        report.battery_info = Some(Battery {
            capacity: 10,
            discharging: false,
        });
        let notices = plan_notices(
            &[],
            &[],
            &report,
            &History::default(),
            &fresh_habits(),
            5,
            &quiet_sys(),
        );
        assert!(notices.is_empty());
    }

    #[test]
    fn demand_spike_against_habit_baseline_fires_v02() {
        let mut habits = fresh_habits();
        for _ in 0..20 {
            habits.observe_at(9, 30.0, 40.0, 0.2, &[]);
        }
        let apps = vec![app("ffmpeg", "Ffmpeg", 300_000, 92.0)];
        let intents = intents_of(&apps);
        let report = report_with_used_pct(30.0);
        // load 2.6 on 4 cores = 65% vs a 30% learned baseline
        let sys = SysSample {
            ncpu: 4,
            load1: 2.6,
            hour: 9,
            ..quiet_sys()
        };
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &habits,
            5,
            &sys,
        );
        assert_eq!(valves(&notices), vec!["V-02"]);
        let v2 = &notices[0];
        assert_eq!(v2.level, Level::Advisory);
        assert!(v2.obs[0].contains("load 65% — your usual here is 30%"));
        assert!(v2.obs.iter().any(|o| o.contains("primary source: Ffmpeg")));

        // same hour at the learned baseline: no spike
        let calm = SysSample {
            ncpu: 4,
            load1: 1.0,
            hour: 9,
            ..quiet_sys()
        };
        let notices = plan_notices(
            &apps,
            &intents,
            &report,
            &History::default(),
            &habits,
            5,
            &calm,
        );
        assert!(notices.iter().all(|n| n.valve != "V-02"));
    }

    #[test]
    fn next_hour_demand_front_forecast_fires_v06() {
        let mut habits = fresh_habits();
        for _ in 0..20 {
            habits.observe_at(10, 50.0, 80.0, 0.2, &["build".to_string()]);
        }
        let report = report_with_used_pct(40.0);
        let notices = plan_notices(
            &[],
            &[],
            &report,
            &History::default(),
            &habits,
            5,
            &quiet_sys(), // hour 9 -> forecast looks at slot 10
        );
        assert_eq!(valves(&notices), vec!["V-06"]);
        let v6 = &notices[0];
        assert_eq!(v6.level, Level::Advisory);
        assert_eq!(v6.title, "DEMAND FRONT INCOMING");
        assert!(v6.obs[0].contains("your 10:00 block usually runs ~80% RAM (now 40%)"));
        assert!(v6.obs.iter().any(|o| o.contains("usual suspects: build")));
    }

    #[test]
    fn erratic_load_pattern_fires_v05() {
        let report = report_with_used_pct(30.0);
        let mut hist = History::default();
        for i in 0..30 {
            let load = if i % 2 == 0 { 0.2 } else { 3.0 };
            hist.push(&report, load);
        }
        let (mean, cv) = hist.load_stats();
        assert!(mean > 1.0 && cv > 0.55);
        let notices = plan_notices(&[], &[], &report, &hist, &fresh_habits(), 5, &quiet_sys());
        assert_eq!(valves(&notices), vec!["V-05"]);
        assert_eq!(notices[0].title, "ERRATIC DEMAND PATTERN");
    }

    // ---------- history windows ----------

    #[test]
    fn history_window_trims_to_capacity() {
        let mut h = History::default();
        let r = report_with_used_pct(50.0);
        for _ in 0..WINDOW + 25 {
            h.push(&r, 1.0);
        }
        assert_eq!(h.len(), WINDOW);
        assert_eq!(h.swap_used_kb.len(), WINDOW);
        assert_eq!(h.load1.len(), WINDOW);
    }

    #[test]
    fn mem_slope_measures_growth_rate() {
        let mut h = History::default();
        // available shrinks 10_000 kB per 5s tick -> used grows 2_000 kB/s
        for i in 0..10u64 {
            h.push(&base_report(TOTAL_KB, 8_000_000 - i * 10_000), 1.0);
        }
        assert!((h.mem_slope_kb_s(5) - 2_000.0).abs() < 1e-9);
    }

    #[test]
    fn mem_slope_guards_degenerate_windows() {
        let mut h = History::default();
        assert!(h.mem_slope_kb_s(5).abs() < 1e-9); // empty
        h.push(&report_with_used_pct(50.0), 1.0);
        assert!(h.mem_slope_kb_s(5).abs() < 1e-9); // single sample
        h.push(&report_with_used_pct(60.0), 1.0);
        assert!(h.mem_slope_kb_s(0).abs() < 1e-9); // zero interval
    }

    #[test]
    fn swap_growth_tracks_first_to_last() {
        let mut h = History::default();
        assert_eq!(h.swap_growth_kb(), 0);
        for i in 0..4u64 {
            let mut r = report_with_used_pct(50.0);
            r.mem.swap_total_kb = 8_000_000;
            r.mem.swap_free_kb = 2_000_000 - i * 25_000;
            h.push(&r, 1.0);
        }
        assert_eq!(h.swap_growth_kb(), 75_000);
    }

    #[test]
    fn load_stats_reports_mean_and_coefficient_of_variation() {
        let mut h = History::default();
        let (mean, cv) = h.load_stats();
        assert!(mean.abs() < 1e-9 && cv.abs() < 1e-9); // empty

        let r = report_with_used_pct(50.0);
        h.push(&r, 1.0);
        h.push(&r, 3.0);
        let (mean, cv) = h.load_stats();
        assert!((mean - 2.0).abs() < 1e-9);
        assert!((cv - 0.5).abs() < 1e-9);

        // flat load has zero variance even with a full window
        let mut flat = History::default();
        for _ in 0..5 {
            flat.push(&r, 2.0);
        }
        let (mean, cv) = flat.load_stats();
        assert!((mean - 2.0).abs() < 1e-9);
        assert!(cv.abs() < 1e-9);
    }

    // ---------- habit learning ----------

    #[test]
    fn observe_seeds_emas_from_first_sample() {
        let mut h = fresh_habits();
        h.observe_at(9, 40.0, 60.0, 0.3, &["build".to_string()]);
        let s = h.slot(9);
        assert_eq!(s.samples, 1);
        assert!((s.cpu_ema - 40.0).abs() < 1e-9);
        assert!((s.mem_ema - 60.0).abs() < 1e-9);
        assert!((s.var_ema - 0.3).abs() < 1e-9);
        assert_eq!(s.top, vec![("build".to_string(), 1)]);
        // every other slot stays untouched
        assert_eq!(h.slot(10).samples, 0);
        assert_eq!(h.total_samples(), 1);
    }

    #[test]
    fn observe_learns_fast_early_then_slows() {
        let mut h = fresh_habits();
        h.observe_at(9, 50.0, 0.0, 1.0, &[]);
        // samples < 50 -> alpha 0.2
        h.observe_at(9, 100.0, 0.0, 0.0, &[]);
        assert!((h.slot(9).cpu_ema - 60.0).abs() < 1e-9); // 50*0.8 + 100*0.2
        assert!((h.slot(9).var_ema - 0.8).abs() < 1e-9); // 1.0*0.8 + 0.0*0.2

        // drive to exactly 50 samples with a steady signal
        let mut steady = fresh_habits();
        for _ in 0..50 {
            steady.observe_at(9, 10.0, 10.0, 0.1, &[]);
        }
        assert_eq!(steady.slot(9).samples, 50);
        // samples >= 50 -> alpha 0.05: 10*0.95 + 110*0.05 = 15.0
        steady.observe_at(9, 110.0, 10.0, 0.1, &[]);
        assert!((steady.slot(9).cpu_ema - 15.0).abs() < 1e-9);
    }

    #[test]
    fn observe_tracks_top_categories_capped_at_three() {
        let mut h = fresh_habits();
        for _ in 0..3 {
            h.observe_at(9, 0.0, 0.0, 0.0, &["build".to_string()]);
        }
        for _ in 0..2 {
            h.observe_at(9, 0.0, 0.0, 0.0, &["browser".to_string()]);
        }
        h.observe_at(9, 0.0, 0.0, 0.0, &["shell".to_string()]);
        h.observe_at(9, 0.0, 0.0, 0.0, &["encode".to_string()]);
        // one observe can count several categories at once
        h.observe_at(
            9,
            0.0,
            0.0,
            0.0,
            &["build".to_string(), "shell".to_string()],
        );

        let top = &h.slot(9).top;
        assert_eq!(top.len(), 3);
        assert_eq!(top[0], ("build".to_string(), 4));
        assert_eq!(top[1], ("browser".to_string(), 2));
        assert_eq!(top[2], ("shell".to_string(), 2));
    }

    #[test]
    fn slots_learn_independently_per_hour() {
        let mut h = fresh_habits();
        h.observe_at(9, 80.0, 0.0, 0.0, &[]);
        h.observe_at(21, 10.0, 0.0, 0.0, &[]);
        assert!((h.slot(9).cpu_ema - 80.0).abs() < 1e-9);
        assert!((h.slot(21).cpu_ema - 10.0).abs() < 1e-9);
        assert_eq!(h.slot(10).samples, 0);
        assert_eq!(h.total_samples(), 2);

        // hours wrap like the clock: 33 == 09
        h.observe_at(33, 40.0, 0.0, 0.0, &[]);
        assert_eq!(h.slot(9).samples, 2);
        assert_eq!(h.total_samples(), 3);
    }

    #[test]
    fn habits_toml_roundtrip_preserves_slots() {
        let mut h = fresh_habits();
        h.observe_at(9, 42.0, 55.0, 0.3, &["build".to_string()]);
        h.observe_at(
            9,
            44.0,
            57.0,
            0.1,
            &["build".to_string(), "browser".to_string()],
        );
        h.observe_at(17, 5.0, 20.0, 0.05, &[]);

        let s = toml::to_string_pretty(&h).expect("serialize");
        let mut back: Habits = toml::from_str(&s).expect("deserialize");
        back.slots.resize_with(24, HabitSlot::default); // mirror Habits::load

        assert_eq!(back.slot(9).samples, 2);
        assert!((back.slot(9).cpu_ema - h.slot(9).cpu_ema).abs() < 1e-9);
        assert!((back.slot(9).var_ema - h.slot(9).var_ema).abs() < 1e-9);
        assert_eq!(back.slot(9).top, h.slot(9).top);
        assert_eq!(back.slot(17).samples, 1);
        assert_eq!(back.total_samples(), h.total_samples());
    }
}
