//! Terminal rendering: ANSI colors, pressure bars. No deps.

pub struct Theme {
    pub enabled: bool,
}

impl Theme {
    pub fn new() -> Self {
        let force = std::env::var("PV_COLOR")
            .map(|v| v == "always")
            .unwrap_or(false);
        Theme {
            enabled: force || (std::env::var("NO_COLOR").is_err() && atty_stdout()),
        }
    }
    pub fn paint(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    pub fn magenta(&self, s: &str) -> String {
        self.paint("35", s)
    }

    /// A compact section divider that remains readable without ANSI colour.
    pub fn section(&self, label: &str) -> String {
        let plain = format!("── {label} ─────────────────────────────────────────");
        self.cyan(&plain)
    }

    pub fn title(&self, label: &str, detail: &str) -> String {
        format!("{}  {}", self.bold(label), self.dim(detail))
    }

    pub fn severity(&self, score: u8) -> String {
        let label = if score >= 75 {
            "HOT"
        } else if score >= 45 {
            "WATCH"
        } else {
            "CALM"
        };
        if score >= 75 {
            self.red(label)
        } else if score >= 45 {
            self.yellow(label)
        } else {
            self.green(label)
        }
    }

    pub fn score_colored(&self, score: u8) -> String {
        let bar = bar(score, 10);
        if score >= 75 {
            self.red(&bar)
        } else if score >= 45 {
            self.yellow(&bar)
        } else {
            self.green(&bar)
        }
    }
}

pub fn bar(score: u8, width: usize) -> String {
    let filled = (score as usize * width / 100).min(width);
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '█' } else { '░' });
    }
    s
}

fn atty_stdout() -> bool {
    unsafe { isatty(1) == 1 }
}

extern "C" {
    fn isatty(fd: i32) -> i32;
}
