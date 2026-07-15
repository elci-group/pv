//! Valve notifications: rustic-cyberpunk pressure-valve cards.
//!
//! Every notice is stamped out of the same die — a heavy plate frame,
//! a stencil level tag, a gauge, observations, and one actionable vent.

use crate::display::Theme;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Advisory,
    Warning,
    Critical,
}

impl Level {
    pub fn tag(&self) -> &'static str {
        match self {
            Level::Advisory => "ADVISORY",
            Level::Warning => "WARNING",
            Level::Critical => "CRITICAL",
        }
    }
    fn cooldown_secs(&self) -> u64 {
        match self {
            Level::Advisory => 15 * 60,
            Level::Warning => 10 * 60,
            Level::Critical => 5 * 60,
        }
    }
}

/// Cooldown gate per valve key; escalation re-fires immediately.
pub struct Cooldowns {
    map: std::collections::HashMap<String, (std::time::Instant, Level)>,
}

impl Cooldowns {
    pub fn new() -> Self {
        Cooldowns { map: std::collections::HashMap::new() }
    }
    pub fn allow(&mut self, key: &str, level: Level) -> bool {
        let now = std::time::Instant::now();
        match self.map.get(key) {
            Some((last, prev)) if level <= *prev => {
                if now.duration_since(*last).as_secs() >= level.cooldown_secs() {
                    self.map.insert(key.to_string(), (now, level));
                    true
                } else {
                    false
                }
            }
            _ => {
                self.map.insert(key.to_string(), (now, level));
                true
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub level: Level,
    pub valve: &'static str, // "V-01" — which valve blew
    pub title: String,
    pub gauge: Option<(String, u8)>, // label, 0..=100
    pub obs: Vec<String>,            // observations
    pub suggest: Option<String>,     // the one thing to do
}

const W: usize = 54; // inner content width

fn gauge_bar(score: u8) -> String {
    let filled = (score as usize * 10 / 100).min(10);
    let mut s = String::with_capacity(10);
    for i in 0..10 {
        s.push(if i < filled { '▰' } else { '▱' });
    }
    s
}

fn pad(content: &str, visible: usize) -> String {
    format!("{content}{}", " ".repeat(W.saturating_sub(visible)))
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn fill(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

/// Split a human-facing line to the card width without cutting Unicode text.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let next = if line.is_empty() {
            word.len()
        } else {
            line.chars().count() + 1 + word.chars().count()
        };
        if next > width && !line.is_empty() {
            out.push(line);
            line = word.to_string();
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// Local wall-clock "HH:MM:SS" without a chrono dependency.
fn tz_offset() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                let s = s.trim().to_string();
                let (sign, rest) = s.split_at(1);
                let h: i64 = rest.get(..2)?.parse().ok()?;
                let m: i64 = rest.get(2..4)?.parse().ok()?;
                Some((h * 3600 + m * 60) * if sign == "-" { -1 } else { 1 })
            })
            .unwrap_or(0)
    })
}

pub fn local_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + tz_offset();
    let day = secs.rem_euclid(86400);
    format!(
        "{:02}:{:02}:{:02}",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

pub fn local_hour() -> usize {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + tz_offset();
    ((secs.rem_euclid(86400)) / 3600) as usize
}

/// Render a notice as a rustic-cyberpunk valve card.
pub fn render(t: &Theme, n: &Notice) -> String {
    let (fc, title_c): (fn(&Theme, &str) -> String, fn(&Theme, &str) -> String) = match n.level {
        Level::Advisory => (Theme::cyan, Theme::bold),
        Level::Warning => (Theme::yellow, Theme::bold),
        Level::Critical => (Theme::red, Theme::bold),
    };
    let mut out: Vec<String> = Vec::new();

    if n.level == Level::Critical {
        let art1 = "   )   (   )   (   )   (   )   (   )   (   )   (";
        let art2 = "  (   )   (   )   (  STEAM OVERPRESSURE  )   (   )";
        out.push(t.red(art1));
        out.push(t.red(art2));
    }

    // header: ╔═[ PV::VALVE ]═[ ▓ ADVISORY ▓ ]═══...═╗
    let head = format!("[ PV::VALVE ]═[ {} {} ]", n.valve, n.level.tag());
    let head_fill = (W + 2).saturating_sub(chars(&head) + 1);
    out.push(format!(
        "{}{}{}",
        fc(t, "╔═"),
        fc(t, &head),
        fc(t, &format!("{}╗", fill('═', head_fill)))
    ));

    let frame = |content: String, visible: usize| -> String {
        format!("{} {} {}", fc(t, "║"), pad(&content, visible), fc(t, "║"))
    };

    // meta line
    let serial = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() & 0xFFFF)
        .unwrap_or(0);
    let meta = format!("{} · local watch · vent 0x{serial:04X}", local_hms());
    out.push(frame(t.dim(&meta), chars(&meta)));
    out.push(frame(String::new(), 0));

    // title
    out.push(frame(title_c(t, &n.title), chars(&n.title)));

    // gauge
    if let Some((label, score)) = &n.gauge {
        let bar_plain = gauge_bar(*score);
        let bar_colored = if t.enabled {
            let code = if *score >= 75 { "31" } else if *score >= 45 { "33" } else { "32" };
            t.paint(code, &bar_plain)
        } else {
            bar_plain.clone()
        };
        let vis = format!("{label} {bar_plain} {score}%");
        out.push(frame(format!("{label} {bar_colored} {score}%"), chars(&vis)));
    }
    out.push(frame(String::new(), 0));

    // observations; long paths and commands stay inside the valve card.
    for o in &n.obs {
        for line in wrap(o, W) {
            out.push(frame(t.dim(&line), chars(&line)));
        }
    }

    // suggestion
    if let Some(s) = &n.suggest {
        out.push(frame(String::new(), 0));
        for (index, line) in wrap(s, W.saturating_sub(2)).into_iter().enumerate() {
            let line = if index == 0 {
                format!("› {line}")
            } else {
                format!("  {line}")
            };
            out.push(frame(title_c(t, &line), chars(&line)));
        }
    }

    // footer
    let foot = "[ end of line ]";
    let foot_fill = (W + 2).saturating_sub(chars(foot) + 1);
    out.push(format!(
        "{}{}{}",
        fc(t, "╚═"),
        fc(t, foot),
        fc(t, &format!("{}╝", fill('═', foot_fill)))
    ));
    out.push(String::new());
    out.join("\n") + "\n"
}

/// Bridge to the desktop via notify-send, if one exists.
pub fn desktop(n: &Notice) {
    static HAVE: OnceLock<bool> = OnceLock::new();
    let have = *HAVE.get_or_init(|| {
        std::process::Command::new("notify-send")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    });
    if !have {
        return;
    }
    let urgency = match n.level {
        Level::Advisory => "low",
        Level::Warning => "normal",
        Level::Critical => "critical",
    };
    let mut body = n.obs.join("\n");
    if let Some(s) = &n.suggest {
        body.push_str(&format!("\n\n› {s}"));
    }
    let _ = std::process::Command::new("notify-send")
        .args(["-u", urgency, &format!("PV :: {}", n.title), &body])
        .spawn();
}
