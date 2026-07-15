//! Pressure detection and prediction: PSI, memory trend, OOM ETA.

use crate::procfs::{self, MemInfo, Psi};

#[derive(Debug, Clone)]
pub struct ResourcePressure {
    pub name: &'static str,
    pub score: u8, // 0..100
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct PressureReport {
    pub cpu: ResourcePressure,
    pub memory: ResourcePressure,
    pub io: ResourcePressure,
    pub battery: Option<ResourcePressure>,
    pub thermal: Option<ResourcePressure>,
    pub mem: MemInfo,
    pub battery_info: Option<procfs::Battery>,
    pub mem_rate_kb_s: f64, // positive = available shrinking
    pub oom_eta_secs: Option<u64>,
    pub overall: u8,
}

/// Sample pressure. Takes two meminfo reads `trend_ms` apart to estimate burn rate.
pub fn measure(trend_ms: u64) -> PressureReport {
    let mem0 = procfs::meminfo();
    let (l1, _l5, _l15) = procfs::loadavg();
    let ncpu = procfs::cpu_count() as f64;
    let psi_cpu = procfs::psi("cpu").unwrap_or_default();
    let psi_mem = procfs::psi("memory").unwrap_or_default();
    let psi_io = procfs::psi("io").unwrap_or_default();
    let batt = procfs::battery();
    let therm = procfs::hottest_thermal();

    std::thread::sleep(std::time::Duration::from_millis(trend_ms));
    let mem1 = procfs::meminfo();

    let mem_rate_kb_s =
        (mem0.available_kb as f64 - mem1.available_kb as f64) / (trend_ms as f64 / 1000.0);

    // CPU score: blend of load-vs-cores and PSI
    let load_score = (l1 / ncpu * 100.0).min(150.0) / 150.0 * 100.0;
    let cpu_score = (load_score * 0.5 + psi_cpu.some_avg10.min(100.0) * 0.5) as u8;

    // Memory score: blend of used% and memory PSI
    let used_pct = (1.0 - mem1.available_kb as f64 / mem1.total_kb.max(1) as f64) * 100.0;
    let mem_score = (used_pct * 0.7 + psi_mem.some_avg10.min(100.0) * 0.3) as u8;

    let io_score = psi_io.some_avg10.min(100.0) as u8;

    // OOM ETA: only when memory is genuinely draining and pressure is real
    let oom_eta_secs = if mem_rate_kb_s > 512.0 && used_pct > 60.0 {
        Some((mem1.available_kb as f64 / mem_rate_kb_s) as u64)
    } else {
        None
    };

    let swap_used = mem1.swap_total_kb.saturating_sub(mem1.swap_free_kb);
    let cpu = ResourcePressure {
        name: "CPU",
        score: cpu_score,
        detail: format!(
            "load {l1:.2} on {ncpu:.0} cores, psi {:.0}%",
            psi_cpu.some_avg10
        ),
    };
    let memory = ResourcePressure {
        name: "RAM",
        score: mem_score,
        detail: format!(
            "{:.1}/{:.1} GB used ({:.0}%), swap {:.1} GB in use, psi {:.0}%",
            (mem1.total_kb - mem1.available_kb) as f64 / 1048576.0,
            mem1.total_kb as f64 / 1048576.0,
            used_pct,
            swap_used as f64 / 1048576.0,
            psi_mem.some_avg10
        ),
    };
    let io = ResourcePressure {
        name: "IO",
        score: io_score,
        detail: format!("psi {:.0}%", psi_io.some_avg10),
    };

    let battery = batt.clone().map(|b| {
        // battery pressure = how close to empty while discharging
        let score = if b.discharging {
            100u8.saturating_sub(b.capacity as u8)
        } else {
            0
        };
        ResourcePressure {
            name: "Battery",
            score,
            detail: format!(
                "{}%{}",
                b.capacity,
                if b.discharging {
                    ", discharging"
                } else {
                    ", on AC"
                }
            ),
        }
    });

    let thermal = therm.map(|t| {
        let score = ((t - 40.0) / 60.0 * 100.0).clamp(0.0, 100.0) as u8;
        ResourcePressure {
            name: "Thermals",
            score,
            detail: format!("{t:.0}°C hottest zone"),
        }
    });

    let overall = [cpu.score, memory.score, io.score]
        .into_iter()
        .max()
        .unwrap_or(0)
        .max(battery.as_ref().map(|b| b.score).unwrap_or(0));

    PressureReport {
        cpu,
        memory,
        io,
        battery,
        thermal,
        mem: mem1,
        battery_info: batt,
        mem_rate_kb_s,
        oom_eta_secs,
        overall,
    }
}

pub fn fmt_eta(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub fn fmt_kb(kb: u64) -> String {
    if kb >= 1048576 {
        format!("{:.1} GB", kb as f64 / 1048576.0)
    } else if kb >= 1024 {
        format!("{:.0} MB", kb as f64 / 1024.0)
    } else {
        format!("{kb} kB")
    }
}

#[allow(dead_code)]
fn _unused(_: Psi) {} // silence unused-import style lints if PSI type unused by callers
