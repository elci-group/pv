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
    fn push(&mut self, r: &PressureReport) {
        self.mem_used_kb
            .push_back(r.mem.total_kb - r.mem.available_kb);
        self.swap_used_kb
            .push_back(r.mem.swap_total_kb - r.mem.swap_free_kb);
        self.load1.push_back(procfs::loadavg().0);
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
        let hour = notify::local_hour();
        let s = &mut self.slots[hour];
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
        s.top.sort_by(|a, b| b.1.cmp(&a.1));
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

/// Valves that only need the current moment — shared by `pv notify`.
pub fn oneshot_notices(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
) -> Vec<Notice> {
    let mut out = Vec::new();
    let used = mem_used_pct(report);
    let recs = recommend::recommend(apps, intents, report);

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
    if let Some(temp) = procfs::hottest_thermal() {
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
fn history_notices(ctx: &Ctx) -> Vec<Notice> {
    let mut out = Vec::new();
    let r = ctx.report;
    let used = mem_used_pct(r);
    let ncpu = procfs::cpu_count() as f64;
    let cpu_pct = procfs::loadavg().0 / ncpu * 100.0;
    let slope = ctx.hist.mem_slope_kb_s(ctx.interval);

    // V-01 escalation: sustained climb before we hit the critical line
    if used >= 72.0 && used < 85.0 && slope > 2048.0 && ctx.hist.len() >= 12 {
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
    let slot = ctx.habits.slot(notify::local_hour());
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
    let psi_mem = procfs::psi("memory").unwrap_or_default();
    if ctx.hist.swap_growth_kb() > 50_000 && psi_mem.some_avg10 > 1.0 {
        out.push(Notice {
            level: Level::Warning,
            valve: "V-04",
            title: "SWAP THRASH RISK".into(),
            gauge: Some(("MEM-PSI".into(), psi_mem.some_avg10.min(100.0) as u8)),
            obs: vec![
                format!(
                    "swap grew {} across the window",
                    fmt_kb(ctx.hist.swap_growth_kb() as u64)
                ),
                format!(
                    "memory stall {:.0}% — the kernel is paging under load",
                    psi_mem.some_avg10
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
    let next = ctx.habits.slot(notify::local_hour() + 1);
    if next.samples >= 20 && next.mem_ema - used > 15.0 {
        out.push(Notice {
            level: Level::Advisory,
            valve: "V-06",
            title: "DEMAND FRONT INCOMING".into(),
            gauge: Some(("RAM".into(), next.mem_ema as u8)),
            obs: vec![
                format!(
                    "your {}:00 block usually runs ~{:.0}% RAM (now {:.0}%)",
                    (notify::local_hour() + 1) % 24,
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

        hist.push(&report);
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

        let mut notices = oneshot_notices(&apps, &intents, &report);
        notices.extend(history_notices(&Ctx {
            hist: &hist,
            habits: &habits,
            apps: &apps,
            intents: &intents,
            report: &report,
            interval,
        }));

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
