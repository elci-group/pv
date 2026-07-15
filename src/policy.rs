//! Pressure policies: user-declared rules evaluated against live state.
//!
//! ~/.config/pv/policies.toml
//! ```toml
//! [[rule]]
//! name = "idle-browsers"
//! category = "browser"
//! min_rss_mb = 200
//! action = "suspend"          # suspend|throttle|notify
//! message = "Suspend {app} — idle and heavy"
//!
//! [[rule]]
//! name = "battery-builds"
//! category = "build"
//! battery_below = 10
//! action = "notify"
//! message = "Battery low — consider `pv migrate {app}`"
//! ```

use std::fs;
use std::path::PathBuf;

use crate::intent::Category;
use crate::pressure::PressureReport;
use crate::procfs::App;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Rule {
    pub name: String,
    pub category: Option<String>,
    pub min_rss_mb: Option<u64>,
    pub min_cpu_pct: Option<f64>,
    pub battery_below: Option<u32>,
    pub min_mem_pressure: Option<u8>,
    pub action: String, // suspend | throttle | notify
    pub message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct PolicyFile {
    #[serde(default)]
    rule: Vec<Rule>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PolicyHit {
    pub rule: String,
    pub app_key: String,
    pub display: String,
    pub action: String,
    pub message: String,
    pub pids: Vec<u32>,
    pub rss_kb: u64,
}

pub fn config_path() -> PathBuf {
    crate::procfs::xdg("XDG_CONFIG_HOME", ".config").join("pv/policies.toml")
}

pub fn load() -> Vec<Rule> {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| toml::from_str::<PolicyFile>(&s).ok())
        .map(|p| p.rule)
        .unwrap_or_default()
}

pub fn default_policy() -> &'static str {
    r#"# Pressure Valve policies — evaluated by `pv policy`
[[rule]]
name = "idle-heavy-browser"
category = "browser"
min_rss_mb = 300
min_mem_pressure = 40
action = "suspend"
message = "Suspend {app} (+{rss_mb} MB)"

[[rule]]
name = "battery-migrate-builds"
category = "build"
battery_below = 10
action = "notify"
message = "Battery {battery}% — migrate {app} to a remote host"

[[rule]]
name = "throttle-backups"
category = "backup"
min_cpu_pct = 40
action = "throttle"
message = "Throttle {app} — competing with foreground work"
"#
}

pub fn evaluate(rules: &[Rule], apps: &[App], categories: &[(String, Category)], report: &PressureReport) -> Vec<PolicyHit> {
    let mut hits = Vec::new();
    for rule in rules {
        // global conditions
        if let Some(mp) = rule.min_mem_pressure {
            if report.memory.score < mp {
                continue;
            }
        }
        if let Some(bb) = rule.battery_below {
            let ok = report
                .battery_info
                .as_ref()
                .map(|b| b.capacity <= bb && b.discharging)
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        for app in apps {
            if let Some(cat) = &rule.category {
                let app_cat = categories
                    .iter()
                    .find(|(k, _)| k == &app.key)
                    .map(|(_, c)| c);
                let want = match cat.to_lowercase().as_str() {
                    "browser" => Category::Browser,
                    "build" => Category::Build,
                    "encode" => Category::Encode,
                    "llm" => Category::Llm,
                    "backup" => Category::Backup,
                    "download" => Category::Download,
                    "shell" => Category::Shell,
                    _ => Category::Unknown,
                };
                if app_cat != Some(&want) {
                    continue;
                }
            }
            if let Some(min) = rule.min_rss_mb {
                if app.rss_kb < min * 1024 {
                    continue;
                }
            }
            if let Some(min) = rule.min_cpu_pct {
                if app.cpu_pct < min {
                    continue;
                }
            }
            let msg = rule
                .message
                .clone()
                .unwrap_or_else(|| format!("{}: {}", rule.action, app.display))
                .replace("{app}", &app.display)
                .replace("{rss_mb}", &(app.rss_kb / 1024).to_string())
                .replace(
                    "{battery}",
                    &report
                        .battery_info
                        .as_ref()
                        .map(|b| b.capacity.to_string())
                        .unwrap_or_else(|| "?".into()),
                );
            hits.push(PolicyHit {
                rule: rule.name.clone(),
                app_key: app.key.clone(),
                display: app.display.clone(),
                action: rule.action.clone(),
                message: msg,
                pids: app.pids.clone(),
                rss_kb: app.rss_kb,
            });
        }
    }
    hits
}
