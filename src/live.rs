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
        .filter(crate::session::is_alive)
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

/// Ambient system facts a frame needs. Gathered once per tick so `render`
/// itself stays a pure function of its inputs (and unit-testable).
struct FrameEnv {
    now: String,            // local clock, HH:MM:SS
    load1: f64,             // 1-minute load average
    cpu_count: usize,       // logical cores
    thermal: Option<f64>,   // hottest thermal zone, °C
    suspended: Vec<String>, // keys of frozen apps
}

impl FrameEnv {
    fn gather() -> Self {
        FrameEnv {
            now: notify::local_hms(),
            load1: procfs::loadavg().0,
            cpu_count: procfs::cpu_count(),
            thermal: procfs::hottest_thermal(),
            suspended: suspend::load_suspended()
                .into_iter()
                .map(|s| s.key)
                .collect(),
        }
    }
}

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
    env: &FrameEnv,
) -> String {
    let inner = w.saturating_sub(4);
    let mut out: Vec<String> = Vec::new();
    let fc = Theme::cyan;

    let head = format!("[ PV::LIVE ]═[ {} ]═[ {}s ]", env.now, interval);
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
    let therm = env
        .thermal
        .map(|t| format!("{t:.0}°C"))
        .unwrap_or_else(|| "--".into());
    let g2 = format!(
        "LOAD {:.2}/{}  MEM {:+.1}MB/s{}  THERM {}  {}",
        env.load1,
        env.cpu_count,
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
    let susp = &env.suspended;
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
        let is_susp = susp.iter().any(|k| k == &app.key);
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
        let env = FrameEnv::gather();
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
            &env,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Category;
    use crate::pressure::ResourcePressure;
    use crate::procfs::{Battery, MemInfo};

    const W: usize = 80;

    fn app(key: &str, display: &str, rss_kb: u64, cpu_pct: f64) -> App {
        App {
            key: key.into(),
            display: display.into(),
            pids: vec![1],
            leader: 1,
            rss_kb,
            cpu_pct,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: String::new(),
            argv: vec![],
            age_secs: 0.0,
            kernel: false,
        }
    }

    fn intent(category: Category) -> Intent {
        Intent {
            category,
            task: String::new(),
            interactive: false,
            can_suspend: true,
            can_interrupt: false,
            can_migrate: false,
            remote_friendly: false,
            gpu: false,
            never_suspend: false,
            detail: String::new(),
        }
    }

    fn report(cpu: u8, mem: u8, io: u8) -> PressureReport {
        let rp = |name: &'static str, score: u8| ResourcePressure {
            name,
            score,
            detail: String::new(),
        };
        PressureReport {
            cpu: rp("CPU", cpu),
            memory: rp("RAM", mem),
            io: rp("IO", io),
            battery: None,
            thermal: None,
            mem: MemInfo {
                total_kb: 16_000_000,
                available_kb: 8_000_000,
                swap_total_kb: 4_000_000,
                swap_free_kb: 4_000_000,
            },
            battery_info: Some(Battery {
                capacity: 85,
                discharging: true,
            }),
            mem_rate_kb_s: -1024.0, // available growing by 1 MB/s
            oom_eta_secs: None,
            overall: cpu.max(mem).max(io),
        }
    }

    fn env() -> FrameEnv {
        FrameEnv {
            now: "12:34:56".into(),
            load1: 1.5,
            cpu_count: 8,
            thermal: Some(55.0),
            suspended: vec![],
        }
    }

    fn plain() -> Theme {
        Theme { enabled: false }
    }

    fn infer_with(text: &str, streaming: bool, note: &str) -> Infer {
        let mut i = Infer::new();
        i.text = text.into();
        i.streaming = streaming;
        i.note = note.into();
        i
    }

    #[allow(clippy::too_many_arguments)]
    fn frame(
        apps: &[App],
        intents: &[(String, Intent)],
        r: &PressureReport,
        prev_rss: &HashMap<String, u64>,
        infer: &Infer,
        key_present: bool,
        env: &FrameEnv,
    ) -> String {
        render(
            &plain(),
            W,
            24,
            2,
            "llama-test",
            apps,
            intents,
            r,
            prev_rss,
            infer,
            key_present,
            env,
        )
    }

    fn frame_basic(apps: &[App], intents: &[(String, Intent)], infer: &Infer, key: bool) -> String {
        frame(
            apps,
            intents,
            &report(50, 25, 5),
            &HashMap::new(),
            infer,
            key,
            &env(),
        )
    }

    #[test]
    fn frame_box_structure_at_80_cols() {
        let apps = vec![app("firefox", "Firefox", 1_500_000, 25.0)];
        let intents = vec![("firefox".to_string(), intent(Category::Browser))];
        let out = frame_basic(&apps, &intents, &Infer::new(), true);
        let lines: Vec<&str> = out.lines().collect();
        // head + 2 gauges + sep + 1 app + sep + 5 infer rows + note + foot
        assert_eq!(lines.len(), 13);

        let head = lines[0];
        assert!(head.starts_with('╔') && head.ends_with('╗'));
        assert_eq!(head.chars().count(), W);
        assert!(head.contains("[ PV::LIVE ]"));
        assert!(head.contains("[ 12:34:56 ]"));
        assert!(head.contains("[ 2s ]"));

        let foot = lines[12];
        assert!(foot.starts_with('╚') && foot.ends_with('╝'));
        assert_eq!(foot.chars().count(), W);
        assert!(foot.contains("[ q quit ]"));

        assert!(lines[3].starts_with('╟') && lines[3].ends_with('╢'));
        assert!(lines[3].contains("processes"));
        assert!(lines[5].starts_with('╟') && lines[5].ends_with('╢'));
        assert!(lines[5].contains("groq :: llama-test"));

        for (i, l) in lines.iter().enumerate().skip(1).take(11) {
            if l.starts_with('╟') {
                continue;
            }
            assert!(l.starts_with('║') && l.ends_with('║'), "line {i}: {l}");
        }
    }

    #[test]
    fn metrics_rows_show_category_state_rss_and_trend() {
        let apps = vec![
            app("firefox", "Firefox", 1_500_000, 25.0),
            app("code", "Code", 800_000, 0.0),
        ];
        let intents = vec![
            ("firefox".to_string(), intent(Category::Browser)),
            ("code".to_string(), intent(Category::Editor)),
        ];
        let mut prev = HashMap::new();
        prev.insert("firefox".to_string(), 1_400_000u64); // grew 100 MB -> up
        prev.insert("code".to_string(), 900_000u64); // shrank 100 MB -> down

        let out = frame(
            &apps,
            &intents,
            &report(50, 25, 5),
            &prev,
            &Infer::new(),
            true,
            &env(),
        );

        let fx = out.lines().find(|l| l.contains("Firefox")).expect("fx row");
        assert_eq!(fx.chars().count(), W);
        assert!(fx.contains("browser"));
        assert!(fx.contains("25%"));
        assert!(fx.contains(&fmt_kb(1_500_000)));
        assert!(fx.contains('▲'));

        let code = out.lines().find(|l| l.contains("Code")).expect("code row");
        assert_eq!(code.chars().count(), W);
        assert!(code.contains("editor"));
        assert!(code.contains("idle"));
        assert!(code.contains('▼'));
    }

    #[test]
    fn metrics_row_without_intent_shows_question_mark() {
        let apps = vec![app("mystery", "Mystery", 100_000, 10.0)];
        let out = frame_basic(&apps, &[], &Infer::new(), true);
        let row = out.lines().find(|l| l.contains("Mystery")).expect("row");
        assert!(row.contains('?'));
    }

    #[test]
    fn frozen_app_row_shows_frozen_instead_of_cpu() {
        let apps = vec![app("firefox", "Firefox", 1_500_000, 30.0)];
        let mut e = env();
        e.suspended = vec!["firefox".to_string()];
        let out = frame(
            &apps,
            &[],
            &report(50, 25, 5),
            &HashMap::new(),
            &Infer::new(),
            true,
            &e,
        );
        let row = out.lines().find(|l| l.contains("Firefox")).expect("row");
        assert!(row.contains("frozen"));
        assert!(!row.contains("30%"));
    }

    #[test]
    fn table_filters_out_small_idle_apps() {
        let apps = vec![
            app("big", "Big", 100_000, 50.0),
            app("tiny", "Tiny", 5_000, 1.0), // below rss and cpu thresholds
        ];
        let out = frame_basic(&apps, &[], &Infer::new(), true);
        assert!(out.contains("Big"));
        assert!(!out.contains("Tiny"));
    }

    #[test]
    fn gauges_show_score_bars_and_sys_stats() {
        let out = frame_basic(&[], &[], &Infer::new(), true);
        let lines: Vec<&str> = out.lines().collect();

        // scores 50/25/5 -> bars of width 10 with 5/2 (floor) filled blocks
        let g1 = lines[1];
        assert!(g1.contains("CPU"));
        assert!(g1.contains(&crate::display::bar(50, 10)));
        assert!(g1.contains("50%"));
        assert!(g1.contains("RAM"));
        assert!(g1.contains(&crate::display::bar(25, 10)));
        assert!(g1.contains("25%"));
        assert!(g1.contains("IO"));
        assert!(g1.contains(" 5%"));

        let g2 = lines[2];
        assert!(g2.contains("LOAD 1.50/8"));
        assert!(g2.contains("+1.0MB/s"));
        assert!(g2.contains("THERM 55°C"));
        assert!(g2.contains("BAT 85%▼"));
    }

    #[test]
    fn gauges_show_oom_eta_and_absent_battery_or_thermal() {
        let mut r = report(10, 90, 5);
        r.oom_eta_secs = Some(125);
        r.battery_info = None;
        let mut e = env();
        e.thermal = None;
        let out = frame(&[], &[], &r, &HashMap::new(), &Infer::new(), true, &e);
        let g2 = out.lines().nth(2).expect("gauges line 2");
        assert!(g2.contains("OOM~2m 05s"));
        assert!(g2.contains("THERM --"));
        assert!(g2.contains("BAT --"));
    }

    #[test]
    fn infer_panel_disabled_when_key_absent() {
        // `--no_infer` (or a missing key) takes this branch in run_live
        let out = frame_basic(&[], &[], &Infer::new(), false);
        assert!(out.contains("GROQ_API_KEY not set — inference offline, metrics live"));
        assert!(out.contains("set the env var or write ~/.config/pv/groq_api_key"));
        assert!(!out.contains("infer:"));
        // head + 2 gauges + 2 seps + 5 panel rows + empty note + foot
        assert_eq!(out.lines().count(), 12);
    }

    #[test]
    fn infer_panel_connecting_shows_bare_cursor() {
        // stream open, no tokens yet
        let infer = infer_with("", true, "12:34:56 · streaming");
        let out = frame_basic(&[], &[], &infer, true);
        let cursor = out.lines().find(|l| l.contains('▌')).expect("cursor row");
        assert!(cursor.starts_with('║') && cursor.ends_with('║'));
        assert!(out.contains("infer: 12:34:56 · streaming"));
    }

    #[test]
    fn infer_panel_streaming_appends_cursor_to_last_text_line() {
        let infer = infer_with("chrome holds half of ram", true, "12:34:56 · streaming");
        let out = frame_basic(&[], &[], &infer, true);
        let cursor = out.lines().find(|l| l.contains('▌')).expect("cursor row");
        assert!(cursor.contains("chrome holds half of ram▌"));
        assert!(out.contains("infer: 12:34:56 · streaming"));
    }

    #[test]
    fn infer_panel_error_keeps_last_text_and_surfaces_note() {
        let infer = infer_with("previous answer text", false, "inference error: boom");
        let out = frame_basic(&[], &[], &infer, true);
        assert!(out.contains("previous answer text"));
        assert!(out.contains("infer: inference error: boom"));
        assert!(!out.contains('▌'));
    }

    #[test]
    fn infer_panel_wraps_and_truncates_to_five_rows() {
        let long = "word ".repeat(100).trim().to_string();
        let infer = infer_with(&long, false, "done");
        let out = frame_basic(&[], &[], &infer, true);
        let lines: Vec<&str> = out.lines().collect();
        let sep = lines.iter().position(|l| l.contains("groq ::")).unwrap();
        let panel = &lines[sep + 1..sep + 6];
        assert!(panel.iter().all(|l| l.starts_with('║') && l.ends_with('║')));
        assert!(panel.iter().all(|l| l.chars().count() == W));
        // wrapped content respects the inner width
        for l in panel {
            let text = l.trim_matches('║').trim();
            assert!(text.chars().count() <= W - 6);
        }
        assert!(out.contains("infer: done"));
    }

    #[test]
    fn frame_renders_at_narrow_width() {
        let apps = vec![app("firefox", "Firefox", 1_500_000, 25.0)];
        let out = render(
            &plain(),
            60,
            24,
            2,
            "m",
            &apps,
            &[],
            &report(50, 25, 5),
            &HashMap::new(),
            &Infer::new(),
            true,
            &env(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with('╔') && lines[0].ends_with('╗'));
        assert_eq!(lines[0].chars().count(), 60);
        let row = lines.iter().find(|l| l.contains("Firefox")).expect("row");
        assert!(row.contains("25%"));
    }

    #[test]
    fn colored_theme_emits_ansi_and_keeps_content() {
        let t = Theme { enabled: true };
        let apps = vec![app("firefox", "Firefox", 1_500_000, 25.0)];
        let out = render(
            &t,
            W,
            24,
            2,
            "m",
            &apps,
            &[],
            &report(80, 25, 5),
            &HashMap::new(),
            &Infer::new(),
            true,
            &env(),
        );
        assert!(out.contains("\x1b["));
        assert!(out.contains("Firefox"));
        assert!(out.contains("[ PV::LIVE ]"));
        assert!(out.contains("80%"));
    }

    #[test]
    fn wrap_breaks_on_word_boundaries() {
        assert_eq!(
            wrap("aa bb cc", 5),
            vec!["aa bb".to_string(), "cc".to_string()]
        );
        assert_eq!(wrap("", 10), Vec::<String>::new());
        assert_eq!(
            wrap("supercalifragilistic", 5),
            vec!["supercalifragilistic".to_string()]
        );
    }
}
