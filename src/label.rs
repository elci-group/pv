//! Natural-language intent labels: a small, local layer of user context.
//!
//! Labels never suspend or alter a process. They tell recommendation logic
//! which work deserves a temporary grace period and record the user's current
//! overall focus for display.

use std::fs;
use std::path::PathBuf;

use crate::intent::{Category, Intent};
use crate::procfs::App;

const DEFAULT_GRACE_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobalLabel {
    pub prompt: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppLabel {
    pub key: String,
    pub display: String,
    pub prompt: String,
    pub expires_at: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Labels {
    pub global: Option<GlobalLabel>,
    #[serde(default)]
    pub app: Vec<AppLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Global,
    App { key: String, display: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub scope: Scope,
    pub grace_secs: Option<u64>,
    pub rationale: String,
}

pub fn config_path() -> PathBuf {
    crate::procfs::xdg("XDG_CONFIG_HOME", ".config").join("pv/labels.toml")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load() -> Labels {
    let mut labels: Labels = fs::read_to_string(config_path())
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    labels.app.retain(|label| label.expires_at > now());
    labels
}

fn save(labels: &Labels) -> Result<(), String> {
    let path = config_path();
    let parent = path.parent().expect("labels path has a parent");
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let text = toml::to_string_pretty(labels).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| e.to_string())
}

/// Infer scope from direct app references first, then from recognised activity.
/// A tutorial or video is intentionally associated with the largest browser:
/// watching it is foreground work even when the tab is momentarily CPU-idle.
pub fn infer(prompt: &str, apps: &[App], intents: &[(String, Intent)]) -> Decision {
    let text = prompt.to_lowercase();
    let browser = || {
        apps.iter()
            .filter(|app| {
                intents
                    .iter()
                    .find(|(key, _)| key == &app.key)
                    .map(|(_, intent)| matches!(intent.category, Category::Browser))
                    .unwrap_or(false)
            })
            .max_by_key(|app| app.rss_kb)
    };
    let explicit = apps.iter().find(|app| {
        let key = app.key.to_lowercase();
        let display = app.display.to_lowercase();
        text.contains(&key) || (!display.is_empty() && text.contains(&display))
    });
    let watching = ["youtube", "tutorial", "watching", "video", "stream"]
        .iter()
        .any(|needle| text.contains(needle));
    if let Some(app) = explicit.or_else(|| watching.then(browser).flatten()) {
        let reason = if watching && !text.contains(&app.key.to_lowercase()) {
            format!(
                "{} is the highest-memory active browser, matching this viewing activity",
                app.display
            )
        } else {
            format!("your note directly refers to {}", app.display)
        };
        return Decision {
            scope: Scope::App {
                key: app.key.clone(),
                display: app.display.clone(),
            },
            grace_secs: Some(DEFAULT_GRACE_SECS),
            rationale: reason,
        };
    }
    Decision {
        scope: Scope::Global,
        grace_secs: None,
        rationale: "saved as your current global working context".into(),
    }
}

pub fn apply(prompt: &str, apps: &[App], intents: &[(String, Intent)]) -> Result<Decision, String> {
    let decision = infer(prompt, apps, intents);
    let mut labels = load();
    match &decision.scope {
        Scope::Global => {
            labels.global = Some(GlobalLabel {
                prompt: prompt.to_string(),
                created_at: now(),
            });
        }
        Scope::App { key, display } => {
            let expires_at = now() + decision.grace_secs.unwrap_or(DEFAULT_GRACE_SECS);
            labels.app.retain(|label| label.key != *key);
            labels.app.push(AppLabel {
                key: key.clone(),
                display: display.clone(),
                prompt: prompt.to_string(),
                expires_at,
            });
        }
    }
    save(&labels)?;
    Ok(decision)
}

pub fn grace_remaining(labels: &Labels, key: &str) -> Option<u64> {
    labels
        .app
        .iter()
        .find(|label| label.key == key && label.expires_at > now())
        .map(|label| label.expires_at - now())
}

pub fn format_duration(secs: u64) -> String {
    format!("{}m", (secs + 59) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(key: &str, rss_kb: u64) -> App {
        App {
            key: key.into(),
            display: key.into(),
            pids: vec![],
            leader: 0,
            rss_kb,
            cpu_pct: 0.0,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: key.into(),
            argv: vec![],
            age_secs: 0.0,
            kernel: false,
        }
    }

    #[test]
    fn tutorial_graces_the_largest_browser() {
        let apps = vec![app("firefox", 400_000), app("chrome", 900_000)];
        let intents: Vec<(String, Intent)> = apps
            .iter()
            .map(|app| (app.key.clone(), crate::intent::classify(app)))
            .collect();
        let decision = infer("I am watching tutorials on YouTube", &apps, &intents);
        assert_eq!(decision.grace_secs, Some(DEFAULT_GRACE_SECS));
        assert_eq!(
            decision.scope,
            Scope::App {
                key: "chrome".into(),
                display: "chrome".into()
            }
        );
    }

    #[test]
    fn development_note_is_global_context() {
        let decision = infer("I am developing a Python based game today", &[], &[]);
        assert_eq!(decision.scope, Scope::Global);
        assert_eq!(decision.grace_secs, None);
    }
}
