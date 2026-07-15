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

/// Raw system reads behind one pressure measurement. Split from the /proc I/O
/// in `measure` so `score` can be driven by synthetic snapshots in tests.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub mem0: MemInfo, // read before the trend window
    pub mem1: MemInfo, // read after the trend window
    pub load1: f64,
    pub ncpu: f64,
    pub psi_cpu: Psi,
    pub psi_mem: Psi,
    pub psi_io: Psi,
    pub battery: Option<procfs::Battery>,
    pub thermal: Option<f64>,
    pub trend_ms: u64,
}

/// Sample pressure. Takes two meminfo reads `trend_ms` apart to estimate burn rate.
pub fn measure(trend_ms: u64) -> PressureReport {
    let mem0 = procfs::meminfo();
    let (load1, _l5, _l15) = procfs::loadavg();
    let ncpu = procfs::cpu_count() as f64;
    let psi_cpu = procfs::psi("cpu").unwrap_or_default();
    let psi_mem = procfs::psi("memory").unwrap_or_default();
    let psi_io = procfs::psi("io").unwrap_or_default();
    let battery = procfs::battery();
    let thermal = procfs::hottest_thermal();

    std::thread::sleep(std::time::Duration::from_millis(trend_ms));
    let mem1 = procfs::meminfo();

    score(Snapshot {
        mem0,
        mem1,
        load1,
        ncpu,
        psi_cpu,
        psi_mem,
        psi_io,
        battery,
        thermal,
        trend_ms,
    })
}

/// Pure scoring: turn a snapshot into a pressure report.
pub fn score(s: Snapshot) -> PressureReport {
    let mem0 = s.mem0;
    let mem1 = s.mem1;
    let l1 = s.load1;
    let ncpu = s.ncpu;
    let psi_cpu = s.psi_cpu;
    let psi_mem = s.psi_mem;
    let psi_io = s.psi_io;
    let batt = s.battery;
    let therm = s.thermal;
    let trend_ms = s.trend_ms;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::Battery;

    fn mem(total_kb: u64, available_kb: u64) -> MemInfo {
        MemInfo {
            total_kb,
            available_kb,
            swap_total_kb: 1_000,
            swap_free_kb: 1_000,
        }
    }

    /// A quiet, healthy machine: 16 cores, no PSI, three quarters of RAM free.
    fn calm() -> Snapshot {
        Snapshot {
            mem0: mem(16_000, 12_000),
            mem1: mem(16_000, 12_000),
            load1: 0.5,
            ncpu: 16.0,
            psi_cpu: Psi::default(),
            psi_mem: Psi::default(),
            psi_io: Psi::default(),
            battery: None,
            thermal: None,
            trend_ms: 1_000,
        }
    }

    #[test]
    fn calm_system_scores_low_with_no_oom_eta() {
        let r = score(calm());
        assert!(r.cpu.score < 10, "cpu {}", r.cpu.score);
        assert!(r.memory.score < 45, "ram {}", r.memory.score);
        assert_eq!(r.io.score, 0);
        assert!(r.battery.is_none());
        assert!(r.thermal.is_none());
        assert_eq!(r.oom_eta_secs, None);
        assert_eq!(r.mem_rate_kb_s, 0.0);
        assert!(
            r.overall < 45,
            "calm systems stay under the watch threshold"
        );
    }

    #[test]
    fn cpu_score_blends_load_per_core_and_psi() {
        // load capped at 1.5x cores -> load term 100; psi 40 -> 100*.5 + 40*.5
        let r = score(Snapshot {
            load1: 24.0,
            psi_cpu: Psi {
                some_avg10: 40.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.cpu.score, 70);
        assert_eq!(r.cpu.detail, "load 24.00 on 16 cores, psi 40%");

        // PSI alone, idle load average
        let r = score(Snapshot {
            load1: 0.0,
            psi_cpu: Psi {
                some_avg10: 100.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.cpu.score, 50);

        // both terms saturated
        let r = score(Snapshot {
            load1: 100.0,
            psi_cpu: Psi {
                some_avg10: 100.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.cpu.score, 100);
    }

    #[test]
    fn memory_score_blends_used_percent_and_psi() {
        // 90% used * 0.7 + psi 20 * 0.3
        let r = score(Snapshot {
            mem0: mem(1_000, 100),
            mem1: mem(1_000, 100),
            psi_mem: Psi {
                some_avg10: 20.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.memory.score, 69);
        assert!(r.memory.detail.contains("90%"), "{}", r.memory.detail);
        assert!(r.memory.detail.contains("psi 20%"), "{}", r.memory.detail);

        // exhausted memory with maxed PSI saturates at 100
        let r = score(Snapshot {
            mem0: mem(1_000, 0),
            mem1: mem(1_000, 0),
            psi_mem: Psi {
                some_avg10: 100.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.memory.score, 100);
    }

    #[test]
    fn io_score_is_pure_psi_with_clamp() {
        let r = score(Snapshot {
            psi_io: Psi {
                some_avg10: 55.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.io.score, 55);
        assert_eq!(r.io.detail, "psi 55%");
        assert_eq!(r.overall, 55, "io pressure alone lifts the overall score");

        let r = score(Snapshot {
            psi_io: Psi {
                some_avg10: 250.0,
                ..Psi::default()
            },
            ..calm()
        });
        assert_eq!(r.io.score, 100, "clamped at 100");
    }

    #[test]
    fn overall_crosses_watch_and_hot_thresholds() {
        // display.rs labels scores: <45 CALM, 45..75 WATCH, >=75 HOT.
        for (psi, want) in [(44.0, 44u8), (45.0, 45), (74.0, 74), (75.0, 75)] {
            let r = score(Snapshot {
                psi_io: Psi {
                    some_avg10: psi,
                    ..Psi::default()
                },
                ..calm()
            });
            assert_eq!(r.overall, want, "psi {psi}");
        }
    }

    #[test]
    fn thermal_score_scales_but_never_lifts_overall() {
        let r = score(Snapshot {
            thermal: Some(40.0),
            ..calm()
        });
        assert_eq!(r.thermal.as_ref().unwrap().score, 0);

        let r = score(Snapshot {
            thermal: Some(20.0),
            ..calm()
        });
        assert_eq!(
            r.thermal.as_ref().unwrap().score,
            0,
            "clamped at zero below 40°C"
        );

        let r = score(Snapshot {
            thermal: Some(100.0),
            ..calm()
        });
        let t = r.thermal.as_ref().unwrap();
        assert_eq!(t.score, 100);
        assert_eq!(t.detail, "100°C hottest zone");
        assert!(r.overall < 45, "hot thermals alone do not raise overall");
    }

    #[test]
    fn battery_pressure_only_counts_while_discharging() {
        let r = score(Snapshot {
            battery: Some(Battery {
                capacity: 15,
                discharging: true,
            }),
            ..calm()
        });
        let b = r.battery.as_ref().unwrap();
        assert_eq!(b.score, 85);
        assert!(b.detail.contains("discharging"));
        assert_eq!(
            r.overall, 85,
            "a nearly-empty discharging battery dominates"
        );

        let r = score(Snapshot {
            battery: Some(Battery {
                capacity: 15,
                discharging: false,
            }),
            ..calm()
        });
        let b = r.battery.as_ref().unwrap();
        assert_eq!(b.score, 0);
        assert!(b.detail.contains("on AC"));
        assert!(r.overall < 45);
    }

    #[test]
    fn oom_eta_only_when_memory_is_draining_under_pressure() {
        // draining 1000 kB/s with ~69% used -> countdown in seconds
        let r = score(Snapshot {
            mem0: mem(16_000, 6_000),
            mem1: mem(16_000, 5_000),
            ..calm()
        });
        assert_eq!(r.mem_rate_kb_s, 1_000.0);
        assert_eq!(r.oom_eta_secs, Some(5));

        // same drain rate but plenty still free -> no countdown
        let r = score(Snapshot {
            mem0: mem(16_000, 12_000),
            mem1: mem(16_000, 11_000),
            ..calm()
        });
        assert_eq!(r.oom_eta_secs, None, "usage under 60%");

        // nearly full but only a trickle of a drain -> no countdown
        let r = score(Snapshot {
            mem0: mem(16_000, 1_000),
            mem1: mem(16_000, 900),
            ..calm()
        });
        assert_eq!(r.oom_eta_secs, None, "drain under 512 kB/s");

        // memory coming back -> negative rate, no countdown
        let r = score(Snapshot {
            mem0: mem(16_000, 4_000),
            mem1: mem(16_000, 8_000),
            ..calm()
        });
        assert!(r.mem_rate_kb_s < 0.0);
        assert_eq!(r.oom_eta_secs, None);
    }

    #[test]
    fn fmt_eta_covers_seconds_minutes_hours() {
        assert_eq!(fmt_eta(5), "5s");
        assert_eq!(fmt_eta(59), "59s");
        assert_eq!(fmt_eta(60), "1m 00s");
        assert_eq!(fmt_eta(65), "1m 05s");
        assert_eq!(fmt_eta(3599), "59m 59s");
        assert_eq!(fmt_eta(3600), "1h 00m");
        assert_eq!(fmt_eta(3725), "1h 02m");
    }

    #[test]
    fn fmt_kb_scales_units() {
        assert_eq!(fmt_kb(512), "512 kB");
        assert_eq!(fmt_kb(1024), "1 MB");
        assert_eq!(fmt_kb(2048), "2 MB");
        assert_eq!(fmt_kb(1_048_576), "1.0 GB");
        assert_eq!(fmt_kb(3_145_728), "3.0 GB");
    }
}
