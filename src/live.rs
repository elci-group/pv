//! `pv live` — persistent dynamic view: realtime metrics table plus a
//! streaming Groq inference panel. Hand-rolled ANSI TUI, no deps.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use crate::display::Theme;
use crate::groq::{self, GroqEvent};
use crate::intent::Intent;
use crate::notify;
use crate::pressure::{fmt_kb, PressureReport};
use crate::procfs::{self, App};
use crate::suspend;

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sig(_: i32) {
    QUIT.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn ioctl(fd: i32, request: usize, ...) -> i32;
}

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_x: u16,
    ws_y: u16,
}

fn term_size() -> (usize, usize) {
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_x: 0,
        ws_y: 0,
    };
    unsafe {
        ioctl(1, 0x5413 /* TIOCGWINSZ */, &mut ws as *mut Winsize)
    };
    if ws.ws_col == 0 {
        (80, 24)
    } else {
        (ws.ws_col as usize, ws.ws_row as usize)
    }
}

fn run_stty(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("stty")
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

// ------------------------------------------------------------- inference

struct Infer {
    rx: Option<Receiver<GroqEvent>>,
    text: String,
    tokens: u32,
    streaming: bool,
    note: String,
    last_run: Instant,
    backoff_until: Instant,
    last_sig: u64,
}

impl Infer {
    fn new() -> Self {
        Infer {
            rx: None,
            text: String::new(),
            tokens: 0,
            streaming: false,
            note: "warming up".into(),
            last_run: Instant::now() - Duration::from_secs(3600),
            backoff_until: Instant::now(),
            last_sig: 0,
        }
    }

    fn drain(&mut self) {
        let mut done = false;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    GroqEvent::Token(t) => {
                        self.tokens += 1;
                        self.text.push_str(&t);
                    }
                    GroqEvent::Done => {
                        done = true;
                        self.note = format!("{} · {} tok", notify::local_hms(), self.tokens);
                    }
                    GroqEvent::Error(e) => {
                        done = true;
                        self.note = format!("inference error: {e}");
                        self.backoff_until = Instant::now() + Duration::from_secs(60);
                    }
                }
            }
        }
        if done {
            self.streaming = false;
            self.rx = None;
        }
    }

    /// Fire a new inference if due. `sig` captures the meaningful state.
    fn maybe_fire(&mut self, model: &str, key: &str, sig: u64, prompt: String) {
        if self.streaming || Instant::now() < self.backoff_until {
            return;
        }
        let heartbeat = self.last_run.elapsed() > Duration::from_secs(120);
        let changed = sig != self.last_sig;
        let cooldown_ok = self.last_run.elapsed() > Duration::from_secs(20);
        if !(cooldown_ok && (changed || heartbeat)) {
            return;
        }
        self.last_sig = sig;
        self.last_run = Instant::now();
        self.tokens = 0;
        self.text.clear();
        self.streaming = true;
        self.note = format!("{} · streaming", notify::local_hms());
        self.rx = Some(groq::stream(model, SYSTEM_PROMPT, &prompt, key));
    }

    fn fail_offline(&mut self, reason: &str) {
        if self.text.is_empty() {
            self.note = reason.into();
        }
    }
}

const SYSTEM_PROMPT: &str = "You are PV, a pressure-valve process intelligence inside a Linux system monitor. Given a compact system snapshot, reply with 2-4 short plain-text lines, each under 95 characters: what actually matters right now, what is likely to happen next, and the single best action. Use ONLY facts and numbers present in the snapshot — never invent percentages or processes. No markdown, no bullets, no preamble.";

// ------------------------------------------------------------- snapshot for inference

fn infer_prompt(apps: &[App], intents: &[(String, Intent)], r: &PressureReport) -> String {
    let used = (1.0 - r.mem.available_kb as f64 / r.mem.total_kb.max(1) as f64) * 100.0;
    let mut s = format!(
        "RAM {:.0}% ({}/{}) swap {} trend {:+.1}MB/s{} | CPU load {:.2}/{} cores psi{:.0} | IO psi{:.0}",
        used,
        fmt_kb(r.mem.total_kb - r.mem.available_kb),
        fmt_kb(r.mem.total_kb),
        fmt_kb(r.mem.swap_total_kb - r.mem.swap_free_kb),
        -r.mem_rate_kb_s / 1024.0,
        r.oom_eta_secs.map(|e| format!(" OOM~{}", crate::pressure::fmt_eta(e))).unwrap_or_default(),
        procfs::loadavg().0,
        procfs::cpu_count(),
        procfs::psi("cpu").map(|p| p.some_avg10).unwrap_or(0.0),
        procfs::psi("io").map(|p| p.some_avg10).unwrap_or(0.0),
    );
    if let Some(b) = &r.battery_info {
        s.push_str(&format!(
            " | BAT {}%{}",
            b.capacity,
            if b.discharging { " discharging" } else { " AC" }
        ));
    }
    if let Some(t) = procfs::hottest_thermal() {
        s.push_str(&format!(" | THERM {t:.0}C"));
    }
    s.push_str(" | TOP:");
    for app in apps.iter().take(5) {
        let int = intents.iter().find(|(k, _)| k == &app.key).map(|(_, i)| i);
        s.push_str(&format!(
            " {}({:?},{:.0}%cpu,{})",
            app.display,
            int.map(|i| i.category)
                .unwrap_or(crate::intent::Category::Unknown),
            app.cpu_pct,
            fmt_kb(app.rss_kb)
        ));
    }
    let susp = suspend::load_suspended();
    if !susp.is_empty() {
        let names: Vec<String> = susp
            .iter()
            .map(|x| format!("{}({})", x.key, fmt_kb(x.rss_kb)))
            .collect();
        s.push_str(&format!(" | SUSPENDED: {}", names.join(",")));
    }
    let running = crate::session::list()
        .into_iter()
        .filter(|s| crate::session::is_alive(s))
        .count();
    if running > 0 {
        s.push_str(&format!(" | PV-SESSIONS: {running} running"));
    }
    s
}

/// Signature of "meaningful" state — inference re-fires when this changes.
fn state_sig(apps: &[App], r: &PressureReport) -> u64 {
    let mut sig: u64 = 0;
    sig = sig
        .wrapping_mul(31)
        .wrapping_add((r.memory.score / 5) as u64);
    sig = sig.wrapping_mul(31).wrapping_add((r.cpu.score / 10) as u64);
    sig = sig
        .wrapping_mul(31)
        .wrapping_add(r.oom_eta_secs.map(|_| 1).unwrap_or(0));
    sig = sig.wrapping_mul(31).wrapping_add(
        r.battery_info
            .as_ref()
            .map(|b| (b.capacity / 10) as u64)
            .unwrap_or(0),
    );
    for a in apps.iter().take(3) {
        for b in a.key.bytes() {
            sig = sig.wrapping_mul(31).wrapping_add(b as u64);
        }
    }
    sig
}

// ------------------------------------------------------------- rendering

fn vis(s: &str) -> usize {
    s.chars().count()
}

fn pad_to(s: String, visible: usize, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(visible)))
}

fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let need = if cur.is_empty() {
            word.len()
        } else {
            cur.len() + 1 + word.len()
        };
        if need > w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn render(
    t: &Theme,
    w: usize,
    rows: usize,
    interval: u64,
    model: &str,
    apps: &[App],
    intents: &[(String, Intent)],
    r: &PressureReport,
    prev_rss: &HashMap<String, u64>,
    infer: &Infer,
    key_present: bool,
) -> String {
    let inner = w.saturating_sub(4);
    let mut out: Vec<String> = Vec::new();
    let fc = Theme::cyan;

    let head = format!("[ PV::LIVE ]═[ {} ]═[ {}s ]", notify::local_hms(), interval);
    out.push(format!(
        "{}{}{}",
        fc(t, "╔═"),
        fc(t, &head),
        fc(
            t,
            &format!("{}╗", "═".repeat((w - 2).saturating_sub(vis(&head) + 1)))
        )
    ));

    let border = |content: String, visible: usize| -> String {
        format!(
            "{} {} {}",
            fc(t, "║"),
            pad_to(content, visible, inner),
            fc(t, "║")
        )
    };
    let sep = |title: &str| -> String {
        let seg = format!("─[ {title} ]");
        format!(
            "{}{}{}",
            fc(t, "╟"),
            fc(t, &seg),
            fc(
                t,
                &format!("{}╢", "─".repeat((w - 2).saturating_sub(vis(&seg) + 1)))
            )
        )
    };

    // gauges line 1
    let g1 = format!(
        "CPU {} {:>2}%  RAM {} {:>2}%  IO {} {:>2}%",
        t.score_colored(r.cpu.score),
        r.cpu.score,
        t.score_colored(r.memory.score),
        r.memory.score,
        t.score_colored(r.io.score),
        r.io.score
    );
    let g1_plain = format!(
        "CPU {} {:>2}%  RAM {} {:>2}%  IO {} {:>2}%",
        crate::display::bar(r.cpu.score, 8),
        r.cpu.score,
        crate::display::bar(r.memory.score, 8),
        r.memory.score,
        crate::display::bar(r.io.score, 8),
        r.io.score
    );
    out.push(border(g1, vis(&g1_plain)));

    // gauges line 2
    let bat = r
        .battery_info
        .as_ref()
        .map(|b| {
            format!(
                "BAT {}%{}",
                b.capacity,
                if b.discharging { "▼" } else { "▲" }
            )
        })
        .unwrap_or_else(|| "BAT --".into());
    let therm = procfs::hottest_thermal()
        .map(|t| format!("{t:.0}°C"))
        .unwrap_or_else(|| "--".into());
    let g2 = format!(
        "LOAD {:.2}/{}  MEM {:+.1}MB/s{}  THERM {}  {}",
        procfs::loadavg().0,
        procfs::cpu_count(),
        -r.mem_rate_kb_s / 1024.0,
        r.oom_eta_secs
            .map(|e| format!("  OOM~{}", crate::pressure::fmt_eta(e)))
            .unwrap_or_default(),
        therm,
        bat
    );
    out.push(border(t.dim(&g2), vis(&g2)));

    // process table
    out.push(sep("processes"));
    let susp = suspend::load_suspended();
    let max_apps = rows.saturating_sub(16).clamp(3, 12);
    for app in apps
        .iter()
        .filter(|a| a.rss_kb > 8_000 || a.cpu_pct > 2.0)
        .take(max_apps)
    {
        let int = intents.iter().find(|(k, _)| k == &app.key).map(|(_, i)| i);
        let cat = int
            .map(|i| format!("{:?}", i.category).to_lowercase())
            .unwrap_or_else(|| "?".into());
        let trend = match prev_rss.get(&app.key) {
            Some(&p) if app.rss_kb > p + 10_000 => t.red("▲"),
            Some(&p) if app.rss_kb + 10_000 < p => t.green("▼"),
            _ => " ".to_string(),
        };
        let is_susp = susp.iter().any(|s| s.key == app.key);
        let (state_word, state_rendered): (String, String) = if is_susp {
            ("frozen".into(), t.magenta("frozen"))
        } else if app.cpu_pct >= 1.0 {
            let w = format!("{:.0}%", app.cpu_pct);
            (w.clone(), w)
        } else {
            ("idle".into(), t.dim("idle"))
        };
        let name = pad_to(app.display.clone(), vis(&app.display), 16);
        let line = format!(
            "{name} {} {:>6} {:>9} {}",
            pad_to(cat.clone(), vis(&cat), 10),
            pad_to(state_rendered, vis(&state_word), 6),
            fmt_kb(app.rss_kb),
            trend
        );
        let line_vis = 16 + 1 + 10 + 1 + 6 + 1 + 9 + 1 + 1;
        out.push(border(line, line_vis));
    }

    // inference panel
    out.push(sep(&format!("groq :: {model}")));
    let infer_lines = 5;
    if !key_present {
        let msg = "GROQ_API_KEY not set — inference offline, metrics live";
        out.push(border(t.dim(msg), vis(msg)));
        let msg2 = "set the env var or write ~/.config/pv/groq_api_key";
        out.push(border(t.dim(msg2), vis(msg2)));
        for _ in 0..infer_lines - 2 {
            out.push(border(String::new(), 0));
        }
    } else {
        let mut lines = wrap(&infer.text, inner.saturating_sub(2));
        if infer.streaming {
            if let Some(last) = lines.last_mut() {
                last.push('▌');
            } else {
                lines.push("▌".into());
            }
        }
        lines.truncate(infer_lines);
        while lines.len() < infer_lines {
            lines.push(String::new());
        }
        for l in lines {
            out.push(border(t.dim(&l), vis(&l)));
        }
    }
    let note = if key_present {
        format!("infer: {}", infer.note)
    } else {
        String::new()
    };
    out.push(border(t.dim(&note), vis(&note)));

    // footer
    let foot = "[ q quit ]";
    out.push(format!(
        "{}{}{}",
        fc(t, "╚═"),
        fc(t, foot),
        fc(
            t,
            &format!("{}╝", "═".repeat((w - 2).saturating_sub(vis(foot) + 1)))
        )
    ));
    out.join("\n")
}

// ------------------------------------------------------------- main loop

pub fn run_live(t: &Theme, interval: u64, model: &str, no_infer: bool) -> i32 {
    let (mut cols, mut rows) = term_size();
    if cols < 50 || rows < 14 {
        eprintln!("[pv] terminal too small for live mode (need ≥50x14)");
        return 1;
    }
    unsafe {
        let h = on_sig as extern "C" fn(i32) as usize;
        signal(2, h);
        signal(15, h);
    }
    // terminal setup: alt screen, hidden cursor, raw-ish input
    let saved_stty = run_stty(&["-g"]);
    let _ = run_stty(&["-icanon", "-echo", "-isig", "min", "1", "time", "0"]);
    print!("\x1b[?1049h\x1b[?25l");
    let _ = std::io::stdout().flush();

    // stdin reader thread (q / esc / ctrl-c)
    let (tx_in, rx_in) = channel::<u8>();
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = [0u8; 1];
        while lock.read(&mut buf).unwrap_or(0) > 0 {
            if tx_in.send(buf[0]).is_err() {
                break;
            }
        }
    });

    let key = if no_infer { None } else { groq::api_key() };
    if key.is_none() && !no_infer {
        // panel will show the offline notice
    }
    if key.is_some() && !groq::have_curl() {
        eprintln!("[pv] curl not found — inference unavailable");
    }
    let mut infer = Infer::new();
    if key.is_none() {
        infer.fail_offline("offline");
    }
    let mut prev_rss: HashMap<String, u64> = HashMap::new();

    let cleanup = |saved: &Option<String>| {
        if let Some(s) = saved {
            let _ = run_stty(&[s]);
        }
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    };

    loop {
        // input
        while let Ok(b) = rx_in.try_recv() {
            if b == b'q' || b == 0x1b || b == 0x03 {
                QUIT.store(true, Ordering::SeqCst);
            }
        }
        if QUIT.load(Ordering::SeqCst) {
            break;
        }

        let tick = Instant::now();
        let procs = procfs::snapshot(150);
        let apps = procfs::group_apps(procs, 150);
        let intents: Vec<(String, Intent)> = apps
            .iter()
            .map(|a| (a.key.clone(), crate::intent::classify(a)))
            .collect();
        let report = crate::pressure::measure(150);

        // inference
        infer.drain();
        if let Some(k) = &key {
            if groq::have_curl() {
                let prompt = infer_prompt(&apps, &intents, &report);
                let sig = state_sig(&apps, &report);
                infer.maybe_fire(model, k, sig, prompt);
            }
        }

        (cols, rows) = term_size();
        let frame = render(
            t,
            cols.min(110),
            rows,
            interval,
            model,
            &apps,
            &intents,
            &report,
            &prev_rss,
            &infer,
            key.is_some(),
        );
        print!("\x1b[H\x1b[J{frame}");
        let _ = std::io::stdout().flush();

        for a in &apps {
            prev_rss.insert(a.key.clone(), a.rss_kb);
        }

        let elapsed = tick.elapsed();
        let wait = Duration::from_secs(interval).saturating_sub(elapsed);
        if !wait.is_zero() {
            std::thread::sleep(wait.min(Duration::from_millis(500)));
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    cleanup(&saved_stty);
    0
}
