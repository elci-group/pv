//! Recommendations: what would relieve pressure right now?

use crate::intent::{self, Category, Intent};
use crate::label::Labels;
use crate::pressure::{fmt_eta, fmt_kb, PressureReport};
use crate::procfs::App;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Suspend,
    Resume,
    Throttle,
    Migrate,
    Reserve,
    Notify,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // pids is informational for future `pv act`
pub struct Recommendation {
    pub action: Action,
    pub target: String,  // app key or resource
    pub display: String, // human sentence
    pub benefit_kb: u64, // estimated memory reclaimed
    pub confidence: u8,  // 0..100
    pub pids: Vec<u32>,
}

#[cfg(test)]
pub fn recommend(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
) -> Vec<Recommendation> {
    recommend_with_labels(apps, intents, report, &Labels::default())
}

pub fn recommend_with_labels(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
    labels: &Labels,
) -> Vec<Recommendation> {
    let mut out = Vec::new();
    let intent_of = |key: &str| intents.iter().find(|(k, _)| k == key).map(|(_, i)| i);

    let mem_hot = report.memory.score >= 70;
    let mem_warm = report.memory.score >= 45;
    let cpu_hot = report.cpu.score >= 70;

    for app in apps {
        let Some(intent) = intent_of(&app.key) else {
            continue;
        };
        let idle = app.cpu_pct < 1.0 && !app.has_audio;
        let graced = crate::label::grace_remaining(labels, &app.key).is_some();

        // idle browser under memory pressure -> suspend
        if !graced
            && matches!(intent.category, Category::Browser)
            && mem_warm
            && idle
            && app.rss_kb > 150_000
        {
            let conf = intent::suspend_confidence(app, intent, 600);
            out.push(Recommendation {
                action: Action::Suspend,
                target: app.key.clone(),
                display: format!("Suspend {} (+{})", app.display, fmt_kb(app.rss_kb)),
                benefit_kb: app.rss_kb,
                confidence: conf,
                pids: app.pids.clone(),
            });
        }

        // restartable build under battery pressure -> migrate
        if intent.can_migrate
            && intent.remote_friendly
            && report
                .battery
                .as_ref()
                .map(|b| b.score >= 85)
                .unwrap_or(false)
        {
            out.push(Recommendation {
                action: Action::Migrate,
                target: app.key.clone(),
                display: format!("Move {} to a remote host (battery critical)", app.display),
                benefit_kb: app.rss_kb,
                confidence: 80,
                pids: app.pids.clone(),
            });
        }

        // background backup saturating CPU -> throttle
        if matches!(intent.category, Category::Backup | Category::Download)
            && cpu_hot
            && app.cpu_pct > 30.0
        {
            out.push(Recommendation {
                action: Action::Throttle,
                target: app.key.clone(),
                display: format!(
                    "Throttle {} ({:.0}% cpu — nice/ionice it)",
                    app.display, app.cpu_pct
                ),
                benefit_kb: 0,
                confidence: 75,
                pids: app.pids.clone(),
            });
        }

        // LLM actively loaded -> reserve GPU note
        if intent.gpu
            && matches!(intent.category, Category::Llm)
            && (app.rss_kb > 500_000 || app.cpu_pct > 10.0)
        {
            out.push(Recommendation {
                action: Action::Reserve,
                target: app.key.clone(),
                display: format!("Reserve GPU/VRAM for {}", app.display),
                benefit_kb: 0,
                confidence: 90,
                pids: vec![],
            });
        }
    }

    // predicted exhaustion overrides everything
    if let Some(eta) = report.oom_eta_secs {
        if mem_hot && !out.iter().any(|r| r.action == Action::Suspend) {
            if let Some(big) = apps.iter().find(|a| {
                crate::label::grace_remaining(labels, &a.key).is_none()
                    && intent_of(&a.key)
                        .map(|i| i.can_suspend && !i.never_suspend)
                        .unwrap_or(false)
            }) {
                out.insert(
                    0,
                    Recommendation {
                        action: Action::Suspend,
                        target: big.key.clone(),
                        display: format!(
                            "OOM in ~{} — suspend {} (+{})",
                            fmt_eta(eta),
                            big.display,
                            fmt_kb(big.rss_kb)
                        ),
                        benefit_kb: big.rss_kb,
                        confidence: 85,
                        pids: big.pids.clone(),
                    },
                );
            }
        }
    }

    out.sort_by(|a, b| {
        b.benefit_kb
            .cmp(&a.benefit_kb)
            .then(b.confidence.cmp(&a.confidence))
    });
    out.dedup_by(|a, b| a.target == b.target && a.action == b.action);
    out.truncate(5);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure::ResourcePressure;
    use crate::procfs::MemInfo;

    fn app(key: &str, rss_kb: u64, cpu_pct: f64) -> App {
        App {
            key: key.to_string(),
            display: key.to_string(),
            pids: vec![4242],
            leader: 4242,
            rss_kb,
            cpu_pct,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: key.to_string(),
            argv: vec![key.to_string()],
            age_secs: 3_600.0,
            kernel: false,
        }
    }

    fn report(mem_score: u8, cpu_score: u8, oom_eta_secs: Option<u64>) -> PressureReport {
        let rp = |name: &'static str, score: u8| ResourcePressure {
            name,
            score,
            detail: String::new(),
        };
        PressureReport {
            cpu: rp("CPU", cpu_score),
            memory: rp("RAM", mem_score),
            io: rp("IO", 0),
            battery: None,
            thermal: None,
            mem: MemInfo::default(),
            battery_info: None,
            mem_rate_kb_s: 0.0,
            oom_eta_secs,
            overall: mem_score.max(cpu_score),
        }
    }

    fn intents_for(apps: &[App]) -> Vec<(String, Intent)> {
        apps.iter()
            .map(|a| (a.key.clone(), intent::classify(a)))
            .collect()
    }

    #[test]
    fn empty_system_produces_no_recommendations() {
        let recs = recommend(&[], &[], &report(95, 90, Some(10)));
        assert!(recs.is_empty());
    }

    #[test]
    fn idle_browser_under_memory_pressure_gets_suspend() {
        let apps = vec![app("firefox", 500_000, 0.2)];
        let recs = recommend(&apps, &intents_for(&apps), &report(60, 0, None));
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.action, Action::Suspend);
        assert_eq!(r.target, "firefox");
        assert_eq!(r.benefit_kb, 500_000);
        assert_eq!(r.confidence, 80);
        assert!(r.display.starts_with("Suspend firefox (+"), "{}", r.display);
    }

    #[test]
    fn active_grace_excludes_a_browser_from_suspend_recommendations() {
        let apps = vec![app("chrome", 900_000, 0.0)];
        let labels = Labels {
            global: None,
            app: vec![crate::label::AppLabel {
                key: "chrome".into(),
                display: "Chrome".into(),
                prompt: "Watching a tutorial".into(),
                expires_at: u64::MAX,
            }],
        };
        let recs = recommend_with_labels(&apps, &intents_for(&apps), &report(60, 0, None), &labels);
        assert!(recs.is_empty());
    }

    #[test]
    fn busy_or_audio_playing_browser_is_left_alone() {
        let mut audio = app("firefox", 500_000, 0.2);
        audio.has_audio = true;
        for a in [app("firefox", 500_000, 5.0), audio] {
            let apps = [a];
            let recs = recommend(&apps, &intents_for(&apps), &report(90, 0, None));
            assert!(recs.is_empty(), "active browser must not be suspended");
        }
    }

    #[test]
    fn small_browser_below_rss_floor_gets_nothing() {
        let apps = vec![app("firefox", 149_000, 0.1)];
        assert!(recommend(&apps, &intents_for(&apps), &report(60, 0, None)).is_empty());
    }

    #[test]
    fn calm_memory_means_no_suspend_recommendation() {
        let apps = vec![app("firefox", 500_000, 0.1)];
        // memory score 44 sits under the 45-point "warm" gate
        assert!(recommend(&apps, &intents_for(&apps), &report(44, 0, None)).is_empty());
    }

    #[test]
    fn never_suspend_apps_survive_even_predicted_oom() {
        let apps = vec![app("postgres", 2_000_000, 5.0), app("ssh", 50_000, 1.0)];
        let recs = recommend(&apps, &intents_for(&apps), &report(95, 0, Some(20)));
        assert!(
            recs.is_empty(),
            "databases and live sessions are protected: {recs:?}"
        );
    }

    #[test]
    fn predicted_oom_overrides_with_first_suspendable_app() {
        let apps = vec![
            app("postgres", 4_000_000, 5.0),
            app("bigapp", 2_097_152, 0.0),
        ];
        let recs = recommend(&apps, &intents_for(&apps), &report(95, 0, Some(30)));
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.action, Action::Suspend);
        assert_eq!(r.target, "bigapp", "postgres is never_suspend, skipped");
        assert_eq!(r.confidence, 85);
        assert!(r.display.contains("OOM in ~30s"), "{}", r.display);
        assert!(r.display.contains("2.0 GB"), "{}", r.display);
    }

    #[test]
    fn predicted_oom_does_not_double_recommend_suspend() {
        let apps = vec![app("firefox", 500_000, 0.2)];
        let recs = recommend(&apps, &intents_for(&apps), &report(80, 0, Some(30)));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action, Action::Suspend);
        assert_eq!(recs[0].target, "firefox");
    }

    #[test]
    fn restartable_build_migrates_only_on_critical_battery() {
        let apps = vec![app("cargo", 300_000, 40.0)];
        let mut rep = report(10, 10, None);

        rep.battery = Some(ResourcePressure {
            name: "Battery",
            score: 90,
            detail: String::new(),
        });
        let recs = recommend(&apps, &intents_for(&apps), &rep);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action, Action::Migrate);
        assert_eq!(recs[0].confidence, 80);

        // low but not critical -> no migration
        rep.battery = Some(ResourcePressure {
            name: "Battery",
            score: 84,
            detail: String::new(),
        });
        assert!(recommend(&apps, &intents_for(&apps), &rep).is_empty());

        // no battery at all -> no migration
        rep.battery = None;
        assert!(recommend(&apps, &intents_for(&apps), &rep).is_empty());
    }

    #[test]
    fn non_migratable_apps_are_never_migrated() {
        // curl is a Download: throttleable but not migratable
        let apps = vec![app("curl", 100_000, 40.0)];
        let mut rep = report(10, 80, None);
        rep.battery = Some(ResourcePressure {
            name: "Battery",
            score: 99,
            detail: String::new(),
        });
        let recs = recommend(&apps, &intents_for(&apps), &rep);
        assert_eq!(recs.len(), 1, "only the throttle rule fires: {recs:?}");
        assert_eq!(recs[0].action, Action::Throttle);
    }

    #[test]
    fn backup_saturating_cpu_gets_throttled() {
        let apps = vec![app("rsync", 50_000, 45.0)];
        let recs = recommend(&apps, &intents_for(&apps), &report(10, 75, None));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action, Action::Throttle);
        assert_eq!(recs[0].benefit_kb, 0);

        // same app while cpu is merely warm -> nothing
        assert!(recommend(&apps, &intents_for(&apps), &report(10, 69, None)).is_empty());

        // hot cpu but the backup is not the hog -> nothing
        let apps = vec![app("rsync", 50_000, 5.0)];
        assert!(recommend(&apps, &intents_for(&apps), &report(10, 75, None)).is_empty());
    }

    #[test]
    fn loaded_llm_gets_gpu_reserve() {
        let apps = vec![app("ollama", 600_000, 2.0)];
        let recs = recommend(&apps, &intents_for(&apps), &report(10, 10, None));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action, Action::Reserve);
        assert_eq!(recs[0].confidence, 90);
        assert!(
            recs[0].pids.is_empty(),
            "reserve is a note, not an operation"
        );

        // small and quiet -> not actively loaded
        let apps = vec![app("ollama", 100_000, 2.0)];
        assert!(recommend(&apps, &intents_for(&apps), &report(10, 10, None)).is_empty());
    }

    #[test]
    fn recommendations_are_sorted_by_benefit_and_capped_at_five() {
        let keys = ["firefox", "chromium", "chrome", "brave", "vivaldi", "opera"];
        let apps: Vec<App> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| app(k, 200_000 + 100_000 * i as u64, 0.1))
            .collect();
        let recs = recommend(&apps, &intents_for(&apps), &report(60, 0, None));
        assert_eq!(recs.len(), 5, "capped at five");
        assert!(recs.windows(2).all(|w| w[0].benefit_kb >= w[1].benefit_kb));
        assert_eq!(recs[0].target, "opera", "largest consumer first");
        assert!(recs.iter().all(|r| r.action == Action::Suspend));
    }
}
