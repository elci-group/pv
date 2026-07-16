//! Minimal internal logging facade backed by the `log` crate.
//!
//! pv keeps its dependency tree tiny; `log` is the one exception because a
//! shared log level makes every module's diagnostics consistent without
//! inventing a custom macro set.
//!
//! Level is controlled by `PV_LOG`:
//!   off | error | warn | info | debug | trace
//! Default is `info` for `pv daemon`, `warn` otherwise.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Install the stderr logger once. Safe to call multiple times.
pub fn init(default: &str) {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return;
    }
    let level = level_from_env(default);
    let _ = log::set_boxed_logger(Box::new(PvLogger { level }));
    log::set_max_level(level);
}

fn level_from_env(default: &str) -> log::LevelFilter {
    std::env::var("PV_LOG")
        .ok()
        .and_then(|s| parse_level(&s))
        .unwrap_or_else(|| parse_level(default).unwrap_or(log::LevelFilter::Warn))
}

fn parse_level(s: &str) -> Option<log::LevelFilter> {
    s.parse().ok()
}

struct PvLogger {
    level: log::LevelFilter,
}

impl log::Log for PvLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = timestamp();
        let level = record.level();
        let target = record.target();
        let mut stderr = std::io::stderr();
        let _ = writeln!(
            stderr,
            "[pv {ts} {level:<5} {target}] {}",
            record.args()
        );
    }

    fn flush(&self) {}
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hh = (secs / 3600) % 24;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    format!("{hh:02}:{mm:02}:{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_level_accepts_standard_names() {
        for (s, want) in [
            ("off", log::LevelFilter::Off),
            ("error", log::LevelFilter::Error),
            ("warn", log::LevelFilter::Warn),
            ("info", log::LevelFilter::Info),
            ("debug", log::LevelFilter::Debug),
            ("trace", log::LevelFilter::Trace),
        ] {
            assert_eq!(parse_level(s), Some(want), "{s} should parse");
        }
    }

    #[test]
    fn parse_level_rejects_garbage() {
        assert_eq!(parse_level("nonsense"), None);
        assert_eq!(parse_level(""), None);
    }
}
