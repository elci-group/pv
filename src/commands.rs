//! Command implementations — one per CLI verb.

use crate::display::Theme;
use crate::intent::{self, Category, Intent};
use crate::pressure::{self, fmt_eta, fmt_kb, PressureReport};
use crate::procfs::{self, App};
use crate::{migrate, policy, recommend, session, suspend};

const SAMPLE_MS: u64 = 200;

struct State {
    apps: Vec<App>,
    intents: Vec<(String, Intent)>,
    report: PressureReport,
}

fn gather() -> State {
    let procs = procfs::snapshot(SAMPLE_MS);
    let apps = procfs::group_apps(procs, SAMPLE_MS);
    let intents: Vec<(String, Intent)> = apps
        .iter()
        .map(|a| (a.key.clone(), intent::classify(a)))
        .collect();
    let report = pressure::measure(400);
    State { apps, intents, report }
}

fn find_app<'a>(apps: &'a [App], target: &str) -> Option<&'a App> {
    if let Ok(pid) = target.parse::<u32>() {
        return apps.iter().find(|a| a.pids.contains(&pid));
    }
    let t = target.to_lowercase();
    apps.iter()
        .find(|a| a.key == t)
        .or_else(|| apps.iter().find(|a| a.key.contains(&t)))
}

// ---------- dashboard (bare `pv`) ----------

pub fn dashboard(t: &Theme) -> i32 {
    let st = gather();
    println!("{}", t.bold("Pressure"));
    for rp in resource_lines(&st.report) {
        println!(
            "  {:<9} {}  {}",
            rp.name,
            t.score_colored(rp.score),
            t.dim(&rp.detail)
        );
    }
    if let Some(eta) = st.report.oom_eta_secs {
        println!(
            "  {} projected memory exhaustion in {}",
            t.red("⚠"),
            t.bold(&fmt_eta(eta))
        );
    }
    println!();

    println!("{}", t.bold("Processes"));
    print_app_table(t, &st.apps, &st.intents, 12);
    println!();

    let recs = recommend::recommend(&st.apps, &st.intents, &st.report);
    if !recs.is_empty() {
        println!("{}", t.bold("Recommendations"));
        for r in &recs {
            let tag = match r.action {
                recommend::Action::Suspend => t.yellow("suspend"),
                recommend::Action::Migrate => t.cyan("migrate"),
                recommend::Action::Throttle => t.magenta("throttle"),
                recommend::Action::Reserve => t.green("reserve"),
                _ => t.dim("note"),
            };
            println!("  [{}] {} {}", tag, r.display, t.dim(&format!("({}%)", r.confidence)));
        }
    } else {
        println!("{}", t.dim("No recommendations — system is comfortable."));
    }
    0
}

fn resource_lines(r: &PressureReport) -> Vec<&pressure::ResourcePressure> {
    let mut v = vec![&r.cpu, &r.memory, &r.io];
    if let Some(b) = &r.battery {
        v.push(b);
    }
    if let Some(th) = &r.thermal {
        v.push(th);
    }
    v
}

fn print_app_table(t: &Theme, apps: &[App], intents: &[(String, Intent)], limit: usize) {
    let suspended = suspend::load_suspended();
    let mut shown = 0;
    for app in apps.iter().filter(|a| a.rss_kb > 8_000 || a.cpu_pct > 2.0) {
        if shown >= limit {
            break;
        }
        let Some((_, int)) = intents.iter().find(|(k, _)| k == &app.key) else { continue };
        let is_susp = suspended.iter().any(|s| s.key == app.key);
        let (status, styled_status): (String, String) = if is_susp {
            ("suspended".into(), t.magenta("suspended"))
        } else if int.never_suspend && int.interactive {
            ("interactive".into(), t.cyan("interactive"))
        } else if app.cpu_pct < 1.0 {
            ("idle".into(), t.dim("idle"))
        } else {
            (format!("{:.0}% cpu", app.cpu_pct), format!("{:.0}% cpu", app.cpu_pct))
        };
        let _ = status;
        let safe = if int.never_suspend {
            t.red("never suspend")
        } else if int.can_suspend {
            let conf = intent::suspend_confidence(app, int, 0);
            if conf >= 70 {
                t.green(&format!("safe to suspend {conf}%"))
            } else {
                t.yellow("suspend risky")
            }
        } else {
            t.dim("system")
        };
        println!(
            "  {:<18} {:<28} {:<12} {:<10} {}",
            t.bold(&app.display),
            t.dim(&int.task),
            styled_status,
            fmt_kb(app.rss_kb),
            safe,
        );
        shown += 1;
    }
    if shown == 0 {
        println!("  {}", t.dim("(nothing notable running)"));
    }
}

// ---------- ps ----------

pub fn ps(t: &Theme) -> i32 {
    let st = gather();
    println!(
        "{}",
        t.bold(&format!(
            "{:<18} {:<28} {:<12} {:<10} {:<8} {}",
            "APP", "INTENT", "STATE", "RSS", "PIDS", "LIFECYCLE"
        ))
    );
    for app in st.apps.iter().filter(|a| a.rss_kb > 4_000 || a.cpu_pct > 1.0) {
        let Some((_, int)) = st.intents.iter().find(|(k, _)| k == &app.key) else { continue };
        let lifecycle: Vec<&str> = [
            (int.can_suspend && !int.never_suspend).then_some("suspend"),
            int.can_interrupt.then_some("interrupt"),
            int.can_migrate.then_some("migrate"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let state = if app.state == 'T' {
            t.magenta("stopped")
        } else if app.cpu_pct >= 1.0 {
            format!("{:.0}% cpu", app.cpu_pct)
        } else {
            t.dim("idle")
        };
        println!(
            "{:<18} {:<28} {:<12} {:<10} {:<8} {}",
            app.display,
            int.task,
            state,
            fmt_kb(app.rss_kb),
            app.pids.len(),
            t.dim(&lifecycle.join(" · ")),
        );
    }
    0
}

// ---------- pressure ----------

pub fn pressure(t: &Theme) -> i32 {
    let r = pressure::measure(800);
    println!("{}", t.bold("System pressure"));
    for rp in resource_lines(&r) {
        println!(
            "  {:<10} {} {:>3}%  {}",
            rp.name,
            t.score_colored(rp.score),
            rp.score,
            t.dim(&rp.detail)
        );
    }
    println!(
        "  {:<10} {}  {}",
        "Mem trend",
        if r.mem_rate_kb_s > 0.0 { t.yellow("draining") } else { t.green("stable") },
        t.dim(&format!("{:+.1} MB/s available", -r.mem_rate_kb_s / 1024.0))
    );
    if let Some(eta) = r.oom_eta_secs {
        println!(
            "  {} projected memory exhaustion in {}",
            t.red("⚠"),
            t.bold(&fmt_eta(eta))
        );
    }
    let attention = if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
        "graphical session active"
    } else if std::env::var("SSH_CONNECTION").is_ok() {
        "remote session"
    } else {
        "unknown"
    };
    println!("  {:<10} {}", "Attention", t.dim(attention));
    0
}

// ---------- explain ----------

pub fn explain(t: &Theme) -> i32 {
    let st = gather();
    let r = &st.report;
    let mut lines: Vec<String> = Vec::new();

    lines.push(if r.overall < 40 {
        "System is responsive.".into()
    } else if r.overall < 70 {
        "System is under moderate pressure.".into()
    } else {
        "System is under heavy pressure.".into()
    });

    // biggest memory consumer
    if let Some(top) = st.apps.first() {
        let share = top.rss_kb as f64 / r.mem.total_kb.max(1) as f64 * 100.0;
        if share > 5.0 {
            lines.push(format!(
                "High RAM is mostly {} ({}, {:.0}% of memory).",
                top.display,
                fmt_kb(top.rss_kb),
                share
            ));
        }
    }

    let swap_used = r.mem.swap_total_kb.saturating_sub(r.mem.swap_free_kb);
    lines.push(if swap_used > 512_000 {
        format!("Swap is in use ({}) — memory pressure is real.", fmt_kb(swap_used))
    } else {
        "No meaningful swap pressure.".into()
    });

    if let Some(eta) = r.oom_eta_secs {
        lines.push(format!("Projected memory exhaustion in {}.", fmt_eta(eta)));
    }

    let recs = recommend::recommend(&st.apps, &st.intents, r);
    if let Some(first) = recs.first() {
        lines.push(format!("Recommendation: {}.", first.display));
        lines.push(format!("Confidence: {}%.", first.confidence));
    } else {
        lines.push("No action needed.".into());
    }

    for l in lines {
        println!("{l}");
    }
    let _ = t;
    0
}

// ---------- intent ----------

pub fn intent(t: &Theme, cmd: &[String]) -> i32 {
    if cmd.is_empty() {
        eprintln!("usage: pv intent <command...>");
        return 2;
    }
    let i = intent::classify_command(&cmd.join(" "));
    let yn = |b: bool| if b { t.green("yes") } else { t.dim("no") };
    println!("{} {}", t.bold("Task:"), i.task);
    println!("  Category:             {:?}", i.category);
    println!("  Interactive:          {}", yn(i.interactive));
    println!("  Can survive interrupt: {}", yn(i.can_interrupt));
    println!("  Can suspend:          {}", yn(i.can_suspend));
    println!("  Can migrate:          {}", yn(i.can_migrate));
    println!("  Remote friendly:      {}", yn(i.remote_friendly));
    println!("  GPU required:         {}", yn(i.gpu));
    if !i.detail.is_empty() {
        println!("  {}", t.dim(&i.detail));
    }
    0
}

// ---------- run / sessions / attach ----------

pub fn run(t: &Theme, cmd: &[String], remote: Option<String>) -> i32 {
    // a pv-run process has no controlling terminal, so classify as non-interactive
    let i = {
        let mut i = intent::classify_command(&cmd.join(" "));
        i.interactive = false;
        if matches!(i.category, intent::Category::Shell) {
            i.never_suspend = false;
        }
        i
    };
    println!("{} {}", t.bold("Intent:"), i.task);
    if !i.detail.is_empty() {
        println!("  {}", t.dim(&i.detail));
    }
    if let Some(host_name) = remote {
        let hosts = migrate::load_hosts();
        let Some((_, host)) = hosts.iter().find(|(n, _)| n == &host_name) else {
            eprintln!("unknown host '{host_name}' — see `pv hosts`");
            return 1;
        };
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());
        return match migrate::migrate_command(host, cmd, &cwd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[pv] {e}");
                1
            }
        };
    }
    match session::run(cmd, &i.task, &format!("{:?}", i.category)) {
        Ok(s) => {
            println!("{} session {} (pid {})", t.green("▶ detached"), s.id, s.pid);
            println!("  log: {}", s.log.display());
            println!("  reattach with: pv attach {}", s.id);
            0
        }
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

pub fn sessions(t: &Theme) -> i32 {
    let sessions = session::list();
    if sessions.is_empty() {
        println!("{}", t.dim("No pv sessions. Start one with `pv run -- <cmd>`."));
        return 0;
    }
    for s in sessions {
        let alive = session::is_alive(&s);
        let status = if alive { t.green("running") } else { t.dim("finished") };
        let last = session::tail(&s, 1).pop().unwrap_or_default();
        println!(
            "  {} {:<10} {:<24} {}",
            status,
            s.id,
            s.cmd.join(" "),
            t.dim(&format!("pid {}", s.pid))
        );
        if !last.is_empty() {
            println!("    {}", t.dim(&format!("└ {}", truncate(&last, 70))));
        }
    }
    0
}

pub fn attach(t: &Theme, id: &str) -> i32 {
    let Some(s) = session::find(id) else {
        eprintln!("no session matching '{id}'");
        return 1;
    };
    println!(
        "{} {} — {}",
        t.bold("Attaching to"),
        s.id,
        s.cmd.join(" ")
    );
    match session::follow(&s) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

// ---------- suspend / resume / kill ----------

pub fn suspend(t: &Theme, target: &str, force: bool) -> i32 {
    let st = gather();
    let Some(app) = find_app(&st.apps, target) else {
        eprintln!("no running app matching '{target}'");
        return 1;
    };
    let int = intent::classify(app);
    if (int.never_suspend || !int.can_suspend) && !force {
        eprintln!(
            "{} {} — {} (use --force to override)",
            t.red("refusing:"),
            app.display,
            int.detail
        );
        return 1;
    }
    match suspend::suspend(&app.key, &app.display, &app.pids, app.rss_kb, &int.task) {
        Ok(e) => {
            println!(
                "{} {} frozen — {} held, resume with `pv resume {}`",
                t.yellow("❄"),
                e.display,
                fmt_kb(e.rss_kb),
                e.key
            );
            0
        }
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

pub fn resume(t: &Theme, target: &str) -> i32 {
    let t_lower = target.to_lowercase();
    let key = suspend::load_suspended()
        .iter()
        .find(|s| s.key == t_lower || s.key.contains(&t_lower))
        .map(|s| s.key.clone());
    let Some(key) = key else {
        eprintln!("no suspended app matching '{target}'");
        return 1;
    };
    match suspend::resume(&key) {
        Ok((n, rss)) => {
            println!(
                "{} {} thawed ({} processes, {} returned to service)",
                t.green("▶"),
                key,
                n,
                fmt_kb(rss)
            );
            0
        }
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

pub fn kill(t: &Theme, target: &str) -> i32 {
    let t_lower = target.to_lowercase();
    let key = suspend::load_suspended()
        .iter()
        .find(|s| s.key == t_lower || s.key.contains(&t_lower))
        .map(|s| s.key.clone());
    let Some(key) = key else {
        eprintln!("no suspended app matching '{target}' (kill works on suspended apps)");
        return 1;
    };
    match suspend::kill_suspended(&key) {
        Ok(n) => {
            println!("{} {key} terminated ({n} processes)", t.red("✖"));
            0
        }
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

pub fn suspended(t: &Theme) -> i32 {
    let all = suspend::load_suspended();
    if all.is_empty() {
        println!("{}", t.dim("Nothing suspended."));
        return 0;
    }
    for s in all {
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().saturating_sub(s.suspended_at))
            .unwrap_or(0);
        println!(
            "  {:<18} {:<28} {:<10} {}",
            t.bold(&s.display),
            t.dim(&s.task),
            fmt_kb(s.rss_kb),
            t.dim(&format!("frozen {}", fmt_eta(age)))
        );
    }
    0
}

// ---------- policy ----------

pub fn policy(t: &Theme, apply: bool, init: bool) -> i32 {
    if init {
        let path = policy::config_path();
        if path.exists() {
            eprintln!("already exists: {}", path.display());
            return 1;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, policy::default_policy()) {
            Ok(()) => {
                println!("wrote {}", path.display());
                return 0;
            }
            Err(e) => {
                eprintln!("[pv] {e}");
                return 1;
            }
        }
    }
    let rules = policy::load();
    if rules.is_empty() {
        println!(
            "{}",
            t.dim("No policies. Create defaults with `pv policy --init`.")
        );
        return 0;
    }
    let st = gather();
    let categories: Vec<(String, Category)> = st
        .intents
        .iter()
        .map(|(k, i)| (k.clone(), i.category))
        .collect();
    let hits = policy::evaluate(&rules, &st.apps, &categories, &st.report);
    if hits.is_empty() {
        println!("{}", t.dim("No policy conditions met right now."));
        return 0;
    }
    for h in &hits {
        let tag = match h.action.as_str() {
            "suspend" => t.yellow("suspend"),
            "throttle" => t.magenta("throttle"),
            _ => t.cyan("notify"),
        };
        println!("  [{}] {} {}", tag, h.message, t.dim(&format!("({})", h.rule)));
        if apply {
            match h.action.as_str() {
                "suspend" => {
                    let app = st.apps.iter().find(|a| a.key == h.app_key);
                    let int = app.map(intent::classify);
                    if let (Some(app), Some(int)) = (app, int) {
                        if int.can_suspend && !int.never_suspend {
                            match suspend::suspend(&app.key, &app.display, &app.pids, app.rss_kb, &int.task) {
                                Ok(_) => println!("    {}", t.green("applied")),
                                Err(e) => println!("    {}", t.red(&e)),
                            }
                        } else {
                            println!("    {}", t.dim("skipped: protected intent"));
                        }
                    }
                }
                "throttle" => {
                    for &pid in &h.pids {
                        let _ = std::process::Command::new("renice")
                            .args(["+15", "-p", &pid.to_string()])
                            .status();
                        let _ = std::process::Command::new("ionice")
                            .args(["-c", "3", "-p", &pid.to_string()])
                            .status();
                    }
                    println!("    {}", t.green("reniced +15, ionice idle"));
                }
                _ => {}
            }
        }
    }
    if !apply {
        println!("{}", t.dim("\n(dry run — pass --apply to act)"));
    }
    0
}

// ---------- hosts / migrate ----------

pub fn hosts(t: &Theme, init: bool) -> i32 {
    if init {
        let path = crate::procfs::xdg("XDG_CONFIG_HOME", ".config").join("pv/hosts.toml");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if path.exists() {
            eprintln!("already exists: {}", path.display());
            return 1;
        }
        return match std::fs::write(&path, migrate::default_hosts()) {
            Ok(()) => {
                println!("wrote {}", path.display());
                0
            }
            Err(e) => {
                eprintln!("[pv] {e}");
                1
            }
        };
    }
    let hosts = migrate::load_hosts();
    if hosts.is_empty() {
        println!(
            "{}",
            t.dim("No remote hosts configured — edit ~/.config/pv/hosts.toml (template: `pv hosts --init`).")
        );
        return 0;
    }
    for (name, h) in hosts {
        let up = migrate::online(&h.addr);
        let status = if up { t.green("online") } else { t.dim("offline") };
        let probe = if up {
            migrate::probe(&h.addr).unwrap_or_default()
        } else {
            String::new()
        };
        println!(
            "  {:<14} {:<24} {:<9} {}",
            t.bold(&name),
            h.addr,
            status,
            t.dim(&if probe.is_empty() { h.note } else { probe })
        );
    }
    0
}

pub fn migrate(t: &Theme, target: &str, to: Option<String>) -> i32 {
    let hosts = migrate::load_hosts();
    if hosts.is_empty() {
        eprintln!("no remote hosts configured — run `pv hosts --init` and edit it");
        return 1;
    }
    // resolve host
    let host = match &to {
        Some(name) => match hosts.iter().find(|(n, _)| n == name) {
            Some((_, h)) => h.clone(),
            None => {
                eprintln!("unknown host '{name}' — see `pv hosts`");
                return 1;
            }
        },
        None => {
            // pick the first online host
            match hosts.iter().find(|(_, h)| migrate::online(&h.addr)) {
                Some((n, h)) => {
                    println!("[pv] selected host {}", t.bold(n));
                    h.clone()
                }
                None => {
                    eprintln!("no configured host is online");
                    return 1;
                }
            }
        }
    };
    if !migrate::online(&host.addr) {
        eprintln!("host {} is offline", host.addr);
        return 1;
    }

    // session target?
    if let Some(s) = session::find(target) {
        println!(
            "[pv] migrating session {} ({}) → {}",
            s.id,
            s.cmd.join(" "),
            host.addr
        );
        return match migrate::migrate_command(&host, &s.cmd, &s.cwd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[pv] {e}");
                1
            }
        };
    }

    // running app target: must be a restartable intent
    let st = gather();
    let Some(app) = find_app(&st.apps, target) else {
        eprintln!("no session or app matching '{target}'");
        return 1;
    };
    let int = intent::classify(app);
    if !int.can_migrate {
        eprintln!(
            "{} {} — intent '{}' is not migratable ({})",
            t.red("refusing:"),
            app.display,
            int.task,
            if int.detail.is_empty() { "state lives locally" } else { &int.detail }
        );
        return 1;
    }
    let cwd = std::fs::read_link(format!("/proc/{}/cwd", app.leader))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let cmd: Vec<String> = app.cmdline.split_whitespace().map(String::from).collect();
    if cmd.is_empty() {
        eprintln!("cannot reconstruct command line for {}", app.display);
        return 1;
    }
    println!(
        "[pv] migrating {} ({}) from {} → {}",
        app.display,
        int.task,
        cwd,
        host.addr
    );
    println!("{}", t.dim("note: local process keeps running; kill it when the remote run is going"));
    match migrate::migrate_command(&host, &cmd, &cwd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[pv] {e}");
            1
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}
