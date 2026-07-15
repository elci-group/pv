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
        .map(|s| parse_rules(&s))
        .unwrap_or_default()
}

/// Pure parse: rules from TOML text. Any parse error yields no rules,
/// so a half-written config never crashes the evaluator.
fn parse_rules(text: &str) -> Vec<Rule> {
    toml::from_str::<PolicyFile>(text)
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

pub fn evaluate(
    rules: &[Rule],
    apps: &[App],
    categories: &[(String, Category)],
    report: &PressureReport,
) -> Vec<PolicyHit> {
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
                // saturating: a pathological min_rss_mb must skip the rule, not overflow
                if app.rss_kb < min.saturating_mul(1024) {
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure::ResourcePressure;
    use crate::procfs::{Battery, MemInfo};

    fn rule(name: &str) -> Rule {
        Rule {
            name: name.into(),
            category: None,
            min_rss_mb: None,
            min_cpu_pct: None,
            battery_below: None,
            min_mem_pressure: None,
            action: "notify".into(),
            message: None,
        }
    }

    fn rp(score: u8) -> ResourcePressure {
        ResourcePressure {
            name: "test",
            score,
            detail: String::new(),
        }
    }

    /// Synthetic report: memory score `mem_score`, optional
    /// `(capacity, discharging)` battery. Never touches the real system.
    fn report(mem_score: u8, battery: Option<(u32, bool)>) -> PressureReport {
        PressureReport {
            cpu: rp(0),
            memory: rp(mem_score),
            io: rp(0),
            battery: None,
            thermal: None,
            mem: MemInfo::default(),
            battery_info: battery.map(|(capacity, discharging)| Battery {
                capacity,
                discharging,
            }),
            mem_rate_kb_s: 0.0,
            oom_eta_secs: None,
            overall: mem_score,
        }
    }

    fn app(key: &str, rss_kb: u64, cpu_pct: f64) -> App {
        App {
            key: key.into(),
            display: key.into(),
            pids: vec![4321],
            leader: 4321,
            rss_kb,
            cpu_pct,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: key.into(),
            argv: vec![key.into()],
            age_secs: 60.0,
            kernel: false,
        }
    }

    fn categorized(app: &App, cat: Category) -> (String, Category) {
        (app.key.clone(), cat)
    }

    #[test]
    fn parse_rules_reads_full_rule_fields() {
        let text = r#"
[[rule]]
name = "idle-browsers"
category = "browser"
min_rss_mb = 200
action = "suspend"
message = "Suspend {app}"

[[rule]]
name = "battery-builds"
category = "build"
battery_below = 10
min_mem_pressure = 40
min_cpu_pct = 12.5
action = "throttle"
"#;
        let rules = parse_rules(text);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "idle-browsers");
        assert_eq!(rules[0].category.as_deref(), Some("browser"));
        assert_eq!(rules[0].min_rss_mb, Some(200));
        assert_eq!(rules[0].action, "suspend");
        assert_eq!(rules[0].message.as_deref(), Some("Suspend {app}"));
        assert_eq!(rules[1].battery_below, Some(10));
        assert_eq!(rules[1].min_mem_pressure, Some(40));
        assert_eq!(rules[1].min_cpu_pct, Some(12.5));
        assert!(rules[1].message.is_none());
    }

    #[test]
    fn parse_rules_tolerates_empty_garbage_and_unrelated_keys() {
        assert!(parse_rules("").is_empty());
        assert!(parse_rules("[[[ not toml").is_empty());
        // valid TOML, but no [[rule]] table
        assert!(parse_rules("unrelated = 1").is_empty());
    }

    #[test]
    fn parse_rules_requires_name_and_action() {
        // Rule has no serde defaults: one bad rule fails the whole file.
        let text = "[[rule]]\nname = \"x\"\n";
        assert!(parse_rules(text).is_empty());
    }

    #[test]
    fn shipped_default_policy_parses() {
        let rules = parse_rules(default_policy());
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].name, "idle-heavy-browser");
        assert_eq!(rules[0].action, "suspend");
        assert_eq!(rules[1].name, "battery-migrate-builds");
        assert_eq!(rules[1].action, "notify");
        assert_eq!(rules[2].name, "throttle-backups");
        assert_eq!(rules[2].action, "throttle");
    }

    #[test]
    fn evaluate_hits_carry_rule_app_and_action() {
        let mut r = rule("heavy");
        r.min_rss_mb = Some(100);
        r.action = "suspend".into();
        let apps = vec![app("firefox", 400 * 1024, 0.0), app("code", 300 * 1024, 0.0)];
        let hits = evaluate(&[r], &apps, &[], &report(0, None));
        // no category on the rule: both apps match, in app order
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].rule, "heavy");
        assert_eq!(hits[0].app_key, "firefox");
        assert_eq!(hits[0].action, "suspend");
        assert_eq!(hits[0].pids, vec![4321]);
        assert_eq!(hits[0].rss_kb, 400 * 1024);
        assert_eq!(hits[1].app_key, "code");
    }

    #[test]
    fn evaluate_rss_threshold_boundary_is_inclusive() {
        let mut r = rule("edge");
        r.min_rss_mb = Some(1); // 1024 kB
        assert!(evaluate(&[r.clone()], &[app("a", 1023, 0.0)], &[], &report(0, None)).is_empty());
        assert_eq!(
            evaluate(&[r], &[app("a", 1024, 0.0)], &[], &report(0, None)).len(),
            1
        );
    }

    #[test]
    fn evaluate_saturating_rss_threshold_never_overflows() {
        let mut r = rule("pathological");
        r.min_rss_mb = Some(u64::MAX);
        // u64::MAX * 1024 would wrap (or panic in debug); saturating_mul pins
        // the threshold at u64::MAX so any real app is skipped, not matched
        let huge = app("a", 1 << 40, 0.0); // 1 TB in kB
        assert!(evaluate(&[r], &[huge], &[], &report(0, None)).is_empty());
    }

    #[test]
    fn evaluate_cpu_threshold_boundary_is_inclusive() {
        let mut r = rule("cpu");
        r.min_cpu_pct = Some(40.0);
        assert!(evaluate(&[r.clone()], &[app("a", 0, 39.9)], &[], &report(0, None)).is_empty());
        assert_eq!(
            evaluate(&[r], &[app("a", 0, 40.0)], &[], &report(0, None)).len(),
            1
        );
    }

    #[test]
    fn evaluate_mem_pressure_gates_the_whole_rule() {
        let mut r = rule("pressure");
        r.min_mem_pressure = Some(40);
        assert!(evaluate(&[r.clone()], &[app("a", 0, 0.0)], &[], &report(39, None)).is_empty());
        assert_eq!(
            evaluate(&[r], &[app("a", 0, 0.0)], &[], &report(40, None)).len(),
            1
        );
    }

    #[test]
    fn evaluate_battery_requires_discharging_at_or_below() {
        let mut r = rule("batt");
        r.battery_below = Some(10);
        let a = || vec![app("a", 0, 0.0)];
        // discharging at the threshold: hit
        assert_eq!(evaluate(&[r.clone()], &a(), &[], &report(0, Some((10, true)))).len(), 1);
        // discharging above the threshold: no hit
        assert!(evaluate(&[r.clone()], &a(), &[], &report(0, Some((11, true)))).is_empty());
        // low but on AC: no hit
        assert!(evaluate(&[r.clone()], &a(), &[], &report(0, Some((5, false)))).is_empty());
        // no battery at all: no hit
        assert!(evaluate(&[r], &a(), &[], &report(0, None)).is_empty());
    }

    #[test]
    fn evaluate_category_matching_is_case_insensitive() {
        let mut r = rule("cat");
        r.category = Some("BrOwSeR".into());
        let browser = app("firefox", 0, 0.0);
        let build = app("cargo", 0, 0.0);
        let cats = vec![
            categorized(&browser, Category::Browser),
            categorized(&build, Category::Build),
        ];
        let hits = evaluate(&[r], &[browser, build], &cats, &report(0, None));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].app_key, "firefox");
    }

    #[test]
    fn evaluate_unlisted_category_string_matches_only_unknown_apps() {
        // "editor" is not in evaluate's match arms, so it maps to Unknown:
        // a real Editor app does NOT match, an Uncategorized one does.
        let mut r = rule("weird");
        r.category = Some("editor".into());
        let editor = app("vim", 0, 0.0);
        let misc = app("thing", 0, 0.0);
        let cats = vec![
            categorized(&editor, Category::Editor),
            categorized(&misc, Category::Unknown),
        ];
        let hits = evaluate(&[r], &[editor, misc], &cats, &report(0, None));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].app_key, "thing");
    }

    #[test]
    fn evaluate_formats_message_placeholders() {
        let mut r = rule("msg");
        r.action = "suspend".into();
        r.message = Some("Suspend {app} (+{rss_mb} MB) batt {battery}".into());
        let mut firefox = app("firefox", 512 * 1024, 0.0);
        firefox.display = "Firefox".into();
        let hits = evaluate(&[r], std::slice::from_ref(&firefox), &[], &report(0, Some((7, true))));
        assert_eq!(hits[0].message, "Suspend Firefox (+512 MB) batt 7");
        // without battery info the placeholder degrades to "?"
        let mut r2 = rule("msg2");
        r2.message = Some("batt {battery}".into());
        let hits = evaluate(&[r2], &[firefox], &[], &report(0, None));
        assert_eq!(hits[0].message, "batt ?");
    }

    #[test]
    fn evaluate_default_message_is_action_and_display() {
        let mut r = rule("nomsg");
        r.action = "throttle".into();
        let hits = evaluate(&[r], &[app("firefox", 0, 0.0)], &[], &report(0, None));
        assert_eq!(hits[0].message, "throttle: firefox");
    }

    #[test]
    fn evaluate_selects_suspend_throttle_notify_actions() {
        let apps = vec![app("a", 0, 0.0)];
        for action in ["suspend", "throttle", "notify"] {
            let mut r = rule("act");
            r.action = action.into();
            let hits = evaluate(&[r], &apps, &[], &report(0, None));
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].action, action);
        }
    }
}
