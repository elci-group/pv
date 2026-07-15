//! Recommendations: what would relieve pressure right now?

use crate::intent::{self, Category, Intent};
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

pub fn recommend(
    apps: &[App],
    intents: &[(String, Intent)],
    report: &PressureReport,
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

        // idle browser under memory pressure -> suspend
        if matches!(intent.category, Category::Browser) && mem_warm && idle && app.rss_kb > 150_000
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
                intent_of(&a.key)
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
