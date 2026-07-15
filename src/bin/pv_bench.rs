//! pv_bench — simulated agentic-workflow benchmark for Groq model selection.
//!
//! Nothing here touches real hardware or the network: every workload is a
//! pre-defined faux snapshot (process lists, context sizes, event rates) and
//! every model row is a static data table. The point is a cost-benefit
//! analysis (CBA) of two inference architectures for agentic supervision:
//!
//!   A) time-cached non-streaming — refire on meaningful state change or a
//!      120 s heartbeat (pv's real rule), one non-streaming call, atomic swap
//!   B) low-latency streaming — refire on a fixed short interval, tokens
//!      stream into the panel as they arrive
//!
//! Questions answered:
//!   1. best models for accuracy and price-performance per workload scale
//!   2. where larger-parameter models become *required* (accuracy cliff)
//!   3. is the cached non-streaming architecture stable enough to compete?
//!   4. a recommended baseline set-up
//!
//! Data sources: pricing + speed from groq.com/pricing and
//! console.groq.com/docs/models (fetched 2026-07-15). Quality indices are
//! labelled estimates from public leaderboards; workload shapes are
//! synthetic. All assumptions are printed with the report.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Static data tables
// ---------------------------------------------------------------------------

struct Model {
    id: &'static str,
    /// effective capability tier (1 = small chat … 4 = frontier-class)
    tier: f64,
    /// output tokens/sec (Groq marketing figures, 2026-07)
    tok_s: u32,
    /// estimated time to first token, seconds
    ttft_s: f64,
    /// USD per 1M input / output tokens
    price_in: f64,
    price_out: f64,
    /// agentic-usefulness index 0-100 (ESTIMATE — public leaderboard blend)
    quality: f64,
    /// output-token multiplier vs. a terse baseline (reasoning models > 1)
    verbosity: f64,
    /// response-to-response variance at fixed input (display churn under B)
    variance: f64,
}

const MODELS: &[Model] = &[
    Model { id: "llama-3.1-8b-instant",                  tier: 1.0, tok_s: 840,  ttft_s: 0.22, price_in: 0.05,  price_out: 0.08, quality: 42.0, verbosity: 0.9, variance: 0.10 },
    Model { id: "meta-llama/llama-4-scout-17b-16e-instruct", tier: 2.0, tok_s: 594, ttft_s: 0.25, price_in: 0.11, price_out: 0.34, quality: 55.0, verbosity: 1.0, variance: 0.07 },
    Model { id: "openai/gpt-oss-20b",                    tier: 2.5, tok_s: 1000, ttft_s: 0.30, price_in: 0.075, price_out: 0.30, quality: 65.0, verbosity: 1.8, variance: 0.06 },
    Model { id: "qwen/qwen3-32b",                        tier: 3.0, tok_s: 662,  ttft_s: 0.30, price_in: 0.29,  price_out: 0.59, quality: 63.0, verbosity: 1.6, variance: 0.07 },
    Model { id: "qwen/qwen3.6-27b",                      tier: 3.0, tok_s: 500,  ttft_s: 0.30, price_in: 0.60,  price_out: 3.00, quality: 66.0, verbosity: 1.5, variance: 0.06 },
    Model { id: "llama-3.3-70b-versatile",               tier: 3.0, tok_s: 394,  ttft_s: 0.35, price_in: 0.59,  price_out: 0.79, quality: 57.0, verbosity: 1.0, variance: 0.06 },
    Model { id: "openai/gpt-oss-120b",                   tier: 4.0, tok_s: 500,  ttft_s: 0.35, price_in: 0.15,  price_out: 0.60, quality: 76.0, verbosity: 1.7, variance: 0.05 },
    Model { id: "moonshotai/kimi-k2-instruct-0905",      tier: 4.0, tok_s: 250,  ttft_s: 0.45, price_in: 1.00,  price_out: 3.00, quality: 79.0, verbosity: 1.4, variance: 0.05 },
];

struct Workload {
    name: &'static str,
    cores: u32,
    agents: u32,
    /// pre-defined faux process table (ps-style lines, no header)
    procs: &'static [&'static str],
    /// prompt tokens per supervision call (state digest + instructions)
    ctx_in: u32,
    /// baseline completion tokens per call (before model verbosity)
    ctx_out: u32,
    /// meaningful state changes per hour (drives architecture A refires)
    events_hr: f64,
    /// required capability tier (accuracy cliff lives here)
    req_tier: f64,
    /// advice older than this is useless, seconds
    freshness_need_s: f64,
    /// hours per day the workflow is actively supervised
    active_hr_day: f64,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "single-core", cores: 1, agents: 1,
        procs: &[
            "python agent.py --loop research      38%cpu   385MB",
            "sqlite3 .agent/state.db               2%cpu   128MB",
        ],
        ctx_in: 1_800, ctx_out: 220, events_hr: 18.0,
        req_tier: 1.0, freshness_need_s: 120.0, active_hr_day: 6.0,
    },
    Workload {
        name: "dual-core", cores: 2, agents: 2,
        procs: &[
            "python agent.py --role researcher    41%cpu   512MB",
            "python agent.py --role writer        36%cpu   470MB",
            "node tools/sandbox.js                 8%cpu   210MB",
            "redis-server :6379                    1%cpu    60MB",
        ],
        ctx_in: 2_600, ctx_out: 300, events_hr: 26.0,
        req_tier: 2.0, freshness_need_s: 90.0, active_hr_day: 7.0,
    },
    Workload {
        name: "quad-core", cores: 4, agents: 4,
        procs: &[
            "python orchestrator.py               22%cpu   640MB",
            "python worker.py --id w1..w4    4x  55%cpu   2.1GB",
            "cargo build --release               180%cpu   1.3GB",
            "postgres -D var/db                   6%cpu   420MB",
            "python indexer.py --embed           30%cpu   890MB",
        ],
        ctx_in: 4_200, ctx_out: 420, events_hr: 40.0,
        req_tier: 3.0, freshness_need_s: 60.0, active_hr_day: 8.0,
    },
    Workload {
        name: "octo-core", cores: 8, agents: 8,
        procs: &[
            "python orchestrator.py               28%cpu   900MB",
            "python worker.py --id w1..w8    8x 310%cpu   5.6GB",
            "rustc --crate-type lib              220%cpu   2.4GB",
            "postgres -D var/db                   9%cpu   780MB",
            "qdrant                              12%cpu   1.1GB",
            "chromium --headless --render        45%cpu   1.6GB",
        ],
        ctx_in: 7_000, ctx_out: 600, events_hr: 65.0,
        req_tier: 3.5, freshness_need_s: 45.0, active_hr_day: 9.0,
    },
    Workload {
        name: "12-core", cores: 12, agents: 12,
        procs: &[
            "python orchestrator.py               31%cpu   1.2GB",
            "python worker.py --id w1..w12  12x 520%cpu   9.8GB",
            "python verifier.py --vote 3/5        18%cpu   700MB",
            "cargo build --workspace           340%cpu   3.1GB",
            "docker compose up (5 svc)            60%cpu   2.2GB",
            "chromium --headless --render 2x     90%cpu   3.0GB",
            "python llm_indexer.py --repo        140%cpu  4.4GB",
            "ffmpeg -i cast.mp4                   80%cpu   500MB",
        ],
        ctx_in: 11_000, ctx_out: 800, events_hr: 95.0,
        req_tier: 4.0, freshness_need_s: 30.0, active_hr_day: 10.0,
    },
];

// ---------------------------------------------------------------------------
// Architecture models
// ---------------------------------------------------------------------------

/// A's heartbeat: refire at least every 120 s even when nothing changed
/// (pv's real rule), i.e. 30/hr; heartbeats coalesce with event refires.
const HEARTBEATS_HR: f64 = 30.0;
const HEARTBEAT_S: f64 = 120.0;
/// B's fixed refire interval while the panel is open.
const STREAM_INTERVAL_S: f64 = 8.0;
/// Baseline accuracy (0-100) below which advice cannot be acted on.
const BASE_PASS: f64 = 40.0;
/// Extra accuracy demanded per requirement-tier step: supervisor errors
/// compound across agents, so bigger fleets need sharper advice.
const PASS_PER_TIER: f64 = 8.0;
/// Accuracy-fit penalty per tier step below the workload requirement.
const TIER_PENALTY: f64 = 0.15;

fn req_accuracy_tier(req_tier: f64) -> f64 {
    BASE_PASS + PASS_PER_TIER * (req_tier - 1.0)
}

fn req_accuracy(w: &Workload) -> f64 {
    req_accuracy_tier(w.req_tier)
}

/// Tier-fit adjusted accuracy (0-100) of a model against a requirement.
fn accuracy_at(m: &Model, req_tier: f64) -> f64 {
    let fit = if m.tier >= req_tier {
        1.0
    } else {
        (1.0 - TIER_PENALTY * (req_tier - m.tier)).max(0.2)
    };
    (m.quality * fit).min(97.0)
}

/// USD per supervision call at a given context shape.
fn call_cost(m: &Model, ctx_in: f64, ctx_out: f64) -> f64 {
    (ctx_in * m.price_in + ctx_out * m.verbosity * m.price_out) / 1e6
}

/// Cheapest model clearing the compounding-error accuracy bar at `req_tier`.
fn cheapest_passing(req_tier: f64, ctx_in: f64, ctx_out: f64) -> Option<&'static Model> {
    let need = req_accuracy_tier(req_tier);
    MODELS
        .iter()
        .filter(|m| accuracy_at(m, req_tier) >= need)
        .min_by(|a, b| {
            call_cost(a, ctx_in, ctx_out)
                .partial_cmp(&call_cost(b, ctx_in, ctx_out))
                .unwrap()
        })
}

#[derive(Clone, Copy, PartialEq)]
enum Arch {
    CachedNonStream,
    Streaming,
}

impl Arch {
    fn label(self) -> &'static str {
        match self {
            Arch::CachedNonStream => "cached-nonstream",
            Arch::Streaming => "streaming",
        }
    }
}

struct Outcome {
    calls_hr: f64,
    accuracy: f64,   // 0-100, tier-fit adjusted
    stability: f64,  // 0-1, display determinism / absence of churn
    staleness_s: f64,
    cost_day: f64,
    cba: f64,        // weighted composite 0-1
    pass: bool,
}

fn simulate(m: &Model, w: &Workload, arch: Arch) -> Outcome {
    let out_tokens = w.ctx_out as f64 * m.verbosity;

    let (calls_hr, staleness_s, stability) = match arch {
        Arch::CachedNonStream => {
            // events plus the heartbeats that don't coalesce with an event
            let coalesce = (w.events_hr / HEARTBEATS_HR).min(0.8);
            let calls = w.events_hr + HEARTBEATS_HR * (1.0 - coalesce);
            // mean age of displayed advice: half the mean inter-fire gap,
            // capped at half the heartbeat (the heartbeat bounds worst case)
            let staleness = (1800.0 / w.events_hr).min(HEARTBEAT_S / 2.0);
            // deterministic cache: only real state changes swap the display;
            // residual instability grows with event churn, model variance is
            // mostly absorbed (identical input -> cached output)
            let stability = 0.96 - (w.events_hr / 800.0).min(0.08) - m.variance * 0.3;
            (calls, staleness, stability)
        }
        Arch::Streaming => {
            let calls = 3600.0 / STREAM_INTERVAL_S;
            let staleness = STREAM_INTERVAL_S / 2.0 + m.ttft_s;
            // token-level flicker plus rephrasing churn on every interval
            let stability = 0.78 - m.variance - 0.04;
            (calls, staleness, stability)
        }
    };

    let cost_hr = calls_hr * (w.ctx_in as f64 * m.price_in + out_tokens * m.price_out) / 1e6;
    let cost_day = cost_hr * w.active_hr_day;

    let fit = if m.tier >= w.req_tier {
        1.0
    } else {
        (1.0 - TIER_PENALTY * (w.req_tier - m.tier)).max(0.2)
    };
    let accuracy = (m.quality * fit).min(97.0);

    let fresh_score = if staleness_s <= w.freshness_need_s {
        1.0
    } else {
        (w.freshness_need_s / staleness_s).clamp(0.0, 1.0)
    };
    // $0.50/day reference: free-tier-ish supervision scores ~1, $2/day ~0.2
    let cost_score = 1.0 / (1.0 + cost_day / 0.50);

    let cba = 0.45 * (accuracy / 100.0)
        + 0.25 * stability
        + 0.15 * fresh_score
        + 0.15 * cost_score;

    Outcome {
        calls_hr,
        accuracy,
        stability: stability.clamp(0.0, 0.99),
        staleness_s,
        cost_day,
        cba,
        pass: accuracy >= req_accuracy(w),
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

struct Ranked<'a> {
    model: &'a Model,
    out: Outcome,
}

fn rank<'a>(w: &Workload, arch: Arch) -> Vec<Ranked<'a>> {
    let mut v: Vec<Ranked> = MODELS
        .iter()
        .map(|m| Ranked { model: m, out: simulate(m, w, arch) })
        .collect();
    v.sort_by(|a, b| b.out.cba.partial_cmp(&a.out.cba).unwrap());
    v
}

/// Cheapest ($/day) model that passes the accuracy threshold for (w, arch).
fn cheapest_pass<'a>(w: &Workload, arch: Arch) -> Option<Ranked<'a>> {
    MODELS
        .iter()
        .map(|m| Ranked { model: m, out: simulate(m, w, arch) })
        .filter(|r| r.out.pass)
        .min_by(|a, b| a.out.cost_day.partial_cmp(&b.out.cost_day).unwrap())
}

fn money(x: f64) -> String {
    if x < 0.01 { format!("${:.4}", x) } else { format!("${:.2}", x) }
}

fn h1(s: &mut String, t: &str, md: bool) {
    if md {
        let _ = writeln!(s, "\n## {t}\n");
    } else {
        let pad = 60usize.saturating_sub(t.len());
        let _ = writeln!(s, "\n== {t} {}\n", "=".repeat(pad));
    }
}

fn h2(s: &mut String, t: &str, md: bool) {
    if md {
        let _ = writeln!(s, "\n### {t}\n");
    } else {
        let _ = writeln!(s, "\n-- {t}\n");
    }
}

fn code(s: &mut String, t: &str, md: bool) {
    if md {
        let _ = writeln!(s, "```\n{t}\n```");
    } else {
        let _ = writeln!(s, "{t}");
    }
}

fn build_report(md: bool) -> String {
    let mut s = String::new();

    if md {
        let _ = writeln!(s, "# pv bench — Groq model CBA under simulated agentic workloads\n");
        let _ = writeln!(s, "Generated by `pv_bench` (fully simulated: pre-defined faux workloads, no\nreal CPU/API use). Sources: [Groq pricing](https://groq.com/pricing/),");
        let _ = writeln!(s, "[supported models](https://console.groq.com/docs/models) (2026-07-15).");
        let _ = writeln!(s, "Quality indices are labelled estimates; workload shapes are synthetic.");
    } else {
        let _ = writeln!(s, "pv bench — Groq model CBA under simulated agentic workloads");
        let _ = writeln!(s, "fully simulated: pre-defined faux values, no real CPU/API use");
        let _ = writeln!(s, "sources: groq.com/pricing, console.groq.com/docs/models (2026-07-15)");
    }

    // ---- model table -------------------------------------------------------
    h1(&mut s, "Model table (all Groq text models, 2026-07)", md);
    if md {
        let _ = writeln!(s, "| model | tier | tok/s | ttft | $in/1M | $out/1M | quality* | verbosity |");
        let _ = writeln!(s, "|-------|-----:|------:|-----:|-------:|--------:|---------:|----------:|");
        for m in MODELS {
            let _ = writeln!(s, "| `{}` | {:.1} | {} | {:.2}s | ${:.3} | ${:.2} | {:.0} | {:.1}x |",
                m.id, m.tier, m.tok_s, m.ttft_s, m.price_in, m.price_out, m.quality, m.verbosity);
        }
        let _ = writeln!(s, "\n\\* agentic-usefulness estimate from public leaderboards; verbosity = output-token multiplier (reasoning models emit more).");
        let _ = writeln!(s, "Excluded: whisper/orpheus (audio), prompt-guard/safeguard (safety), groq/compound (systems, passthrough pricing).");
    } else {
        let _ = writeln!(s, "{:<40} {:>4} {:>6} {:>5} {:>8} {:>7} {:>5} {:>4}",
            "model", "tier", "tok/s", "ttft", "$in/1M", "$out/1M", "qual*", "verb");
        for m in MODELS {
            let _ = writeln!(s, "{:<40} {:>4.1} {:>6} {:>4.2}s {:>8.3} {:>7.2} {:>5.0} {:>3.1}x",
                m.id, m.tier, m.tok_s, m.ttft_s, m.price_in, m.price_out, m.quality, m.verbosity);
        }
        let _ = writeln!(s, "* quality = agentic-usefulness estimate; excluded: audio/safety/systems models");
    }

    // ---- assumptions -------------------------------------------------------
    h1(&mut s, "Architectures & assumptions", md);
    code(&mut s, &format!(
"A cached-nonstream  refire = events + {}x 120 s-heartbeats (coalesced), one\n\
 \x20                  non-streaming call, atomic panel swap on completion\n\
B streaming         refire every {} s, tokens displayed as they arrive\n\
accuracy            quality x tier-fit (penalty {:.0}%/tier below requirement)\n\
pass threshold      accuracy >= {:.0} + {:.0}*(req tier - 1) — errors compound\n\
\x20                   across agents, bigger fleets need sharper advice\n\
stability           A: 0.96 - churn(events) - 0.3*variance (cache dedupes)\n\
\x20                   B: 0.78 - variance - 0.04 flicker\n\
staleness           A: min(1800/events, 60)s   B: interval/2 + ttft\n\
cba = 0.45*acc + 0.25*stability + 0.15*freshness + 0.15*cost (ref $0.50/day)",
        HEARTBEATS_HR as u32, STREAM_INTERVAL_S as u32, TIER_PENALTY * 100.0,
        BASE_PASS, PASS_PER_TIER), md);

    // ---- per-workload detail ------------------------------------------------
    h1(&mut s, "Workload results", md);
    for w in WORKLOADS {
        let agents = if w.agents == 1 { "1 agent".to_string() } else { format!("{} agents", w.agents) };
        let cores = if w.cores == 1 { "1 core".to_string() } else { format!("{} cores", w.cores) };
        h2(&mut s, &format!("{} — {agents}, faux ps snapshot", w.name), md);
        let procs = w.procs.join("\n");
        code(&mut s, &procs, md);
        if md {
            let _ = writeln!(s, "{cores} · ctx {}/{} tok · {:.0} events/h · needs tier {:.1}+, freshness ≤ {:.0}s · {:.0} h/day\n",
                w.ctx_in, w.ctx_out, w.events_hr, w.req_tier, w.freshness_need_s, w.active_hr_day);
        } else {
            let _ = writeln!(s, "{cores}, ctx {}/{} tok, {:.0} events/h, needs tier {:.1}+, freshness <= {:.0}s, {:.0} h/day\n",
                w.ctx_in, w.ctx_out, w.events_hr, w.req_tier, w.freshness_need_s, w.active_hr_day);
        }
        for arch in [Arch::CachedNonStream, Arch::Streaming] {
            let ranked = rank(w, arch);
            let _ = writeln!(s, "[{}] calls/h: {:.0}", arch.label(), ranked[0].out.calls_hr);
            if md {
                let _ = writeln!(s, "| # | model | acc | stable | stale | $/day | cba | pass |");
                let _ = writeln!(s, "|--:|-------|----:|-------:|------:|------:|----:|:----:|");
                for (i, r) in ranked.iter().take(4).enumerate() {
                    let _ = writeln!(s, "| {} | `{}` | {:.0} | {:.2} | {:.0}s | {} | {:.3} | {} |",
                        i + 1, r.model.id, r.out.accuracy, r.out.stability,
                        r.out.staleness_s, money(r.out.cost_day), r.out.cba,
                        if r.out.pass { "yes" } else { "NO" });
                }
                let _ = writeln!(s);
            } else {
                let _ = writeln!(s, "  {:<40} {:>4} {:>6} {:>6} {:>8} {:>6}  {}",
                    "model", "acc", "stable", "stale", "$/day", "cba", "pass");
                for r in ranked.iter().take(4) {
                    let _ = writeln!(s, "  {:<40} {:>4.0} {:>6.2} {:>5.0}s {:>8} {:>6.3}  {}",
                        r.model.id, r.out.accuracy, r.out.stability,
                        r.out.staleness_s, money(r.out.cost_day), r.out.cba,
                        if r.out.pass { "yes" } else { "NO" });
                }
            }
        }
    }

    // ---- accuracy cliff ------------------------------------------------------
    h1(&mut s, "Accuracy cliff — cheapest passing model per workload", md);
    if md {
        let _ = writeln!(s, "| workload | req tier | cached-nonstream | $/day | streaming | $/day |");
        let _ = writeln!(s, "|----------|---------:|------------------|------:|-----------|------:|");
        for w in WORKLOADS {
            let a = cheapest_pass(w, Arch::CachedNonStream);
            let b = cheapest_pass(w, Arch::Streaming);
            let _ = writeln!(s, "| {} | {:.1} | {} | {} | {} | {} |",
                w.name, w.req_tier,
                a.as_ref().map(|r| format!("`{}`", r.model.id)).unwrap_or("none".into()),
                a.as_ref().map(|r| money(r.out.cost_day)).unwrap_or("-".into()),
                b.as_ref().map(|r| format!("`{}`", r.model.id)).unwrap_or("none".into()),
                b.as_ref().map(|r| money(r.out.cost_day)).unwrap_or("-".into()));
        }
    } else {
        let _ = writeln!(s, "{:<12} {:>4}  {:<40} {:>8}  {:<40} {:>8}",
            "workload", "req", "cached-nonstream", "$/day", "streaming", "$/day");
        for w in WORKLOADS {
            let a = cheapest_pass(w, Arch::CachedNonStream);
            let b = cheapest_pass(w, Arch::Streaming);
            let _ = writeln!(s, "{:<12} {:>4.1}  {:<40} {:>8}  {:<40} {:>8}",
                w.name, w.req_tier,
                a.as_ref().map(|r| r.model.id).unwrap_or("none"),
                a.as_ref().map(|r| money(r.out.cost_day)).unwrap_or("-".into()),
                b.as_ref().map(|r| r.model.id).unwrap_or("none"),
                b.as_ref().map(|r| money(r.out.cost_day)).unwrap_or("-".into()));
        }
    }

    // ---- head-to-head --------------------------------------------------------
    h1(&mut s, "Head-to-head: cached-nonstream vs streaming (each workload's cheapest passer)", md);
    let mut verdict_ok = true;
    let mut ratio_sum = 0.0;
    let mut stab_delta_sum = 0.0;
    if md {
        let _ = writeln!(s, "| workload | model | $A/day | $B/day | B/A cost | stab A | stab B | stale A (need) |");
        let _ = writeln!(s, "|----------|-------|-------:|-------:|---------:|-------:|-------:|----------------|");
    } else {
        let _ = writeln!(s, "{:<12} {:<38} {:>8} {:>8} {:>6} {:>7} {:>7} {:>14}",
            "workload", "model", "$A/day", "$B/day", "B/A", "stabA", "stabB", "staleA(need)");
    }
    for w in WORKLOADS {
        let Some(a) = cheapest_pass(w, Arch::CachedNonStream) else { continue };
        let b = simulate(a.model, w, Arch::Streaming);
        let ratio = b.cost_day / a.out.cost_day.max(1e-9);
        ratio_sum += ratio;
        stab_delta_sum += a.out.stability - b.stability;
        if a.out.staleness_s > w.freshness_need_s { verdict_ok = false; }
        if md {
            let _ = writeln!(s, "| {} | `{}` | {} | {} | {:.1}x | {:.2} | {:.2} | {:.0}s ({:.0}s) |",
                w.name, a.model.id, money(a.out.cost_day), money(b.cost_day),
                ratio, a.out.stability, b.stability, a.out.staleness_s, w.freshness_need_s);
        } else {
            let _ = writeln!(s, "{:<12} {:<38} {:>8} {:>8} {:>5.1}x {:>7.2} {:>7.2} {:>7.0}s ({:.0}s)",
                w.name, a.model.id, money(a.out.cost_day), money(b.cost_day),
                ratio, a.out.stability, b.stability, a.out.staleness_s, w.freshness_need_s);
        }
    }
    let n = WORKLOADS.len() as f64;
    let _ = writeln!(s);
    let _ = writeln!(s, "mean cost ratio B/A: {:.1}x — mean stability delta (A-B): +{:.2}",
        ratio_sum / n, stab_delta_sum / n);

    // ---- verdict + baseline ---------------------------------------------------
    h1(&mut s, "Verdict", md);
    if verdict_ok {
        let _ = writeln!(s, "CACHED NON-STREAMING IS STABLE ENOUGH TO COMPETE — and wins on cost.");
        let _ = writeln!(s, "Event-driven refires keep staleness under every workload's freshness");
        let _ = writeln!(s, "need while costing {:.1}x less on average, with ~{:.2} higher display",
            ratio_sum / n, stab_delta_sum / n);
        let _ = writeln!(s, "stability (no token flicker, no rephrase churn). Streaming only pays");
        let _ = writeln!(s, "when decisions must react in <{} s — agentic loops act in seconds,", STREAM_INTERVAL_S as u32);
        let _ = writeln!(s, "so they never enter that regime.");
    } else {
        let _ = writeln!(s, "CACHED NON-STREAMING VIOLATES a freshness need — streaming required");
        let _ = writeln!(s, "for the flagged workloads above.");
    }

    h1(&mut s, "RECOMMENDED BASELINE", md);
    if md { let _ = writeln!(s, "```"); }
    let pick = |wi: usize| cheapest_pass(&WORKLOADS[wi], Arch::CachedNonStream)
        .map(|r| (r.model.id, r.out.cost_day, r.out.accuracy));
    let (single, dual, quad, octo, twelve) =
        (pick(0), pick(1), pick(2), pick(3), pick(4));
    let line = |s: &mut String, what: &str, p: Option<(&str, f64, f64)>| {
        if let Some((id, cost, acc)) = p {
            let _ = writeln!(s, "  {:<14} {:<40} (acc {:.0}, {}/day)", what, id, acc, money(cost));
        } else {
            let _ = writeln!(s, "  {:<14} none pass", what);
        }
    };
    line(&mut s, "default", dual.or(single));
    line(&mut s, "fallback", single);
    line(&mut s, "quad+", quad);
    line(&mut s, "octo", octo);
    line(&mut s, "12-core", twelve);
    let _ = writeln!(s);
    let _ = writeln!(s, "  architecture : cached-nonstream (event + 120s heartbeat), atomic swap");
    let _ = writeln!(s, "  escalation   : move up one row when req tier exceeds the row's model");
    let _ = writeln!(s, "  pv live note : pv's own narration panel is a single-core-class task —");
    let _ = writeln!(s, "                 llama-3.1-8b-instant stays the right default there; the");
    let _ = writeln!(s, "                 rows above govern *agentic workflow supervision*.");
    if md { let _ = writeln!(s, "```"); }

    s
}

// ---------------------------------------------------------------------------

/// Invariants that make the benchmark meaningful; used as a deliver gate.
fn check() -> Result<(), String> {
    for w in WORKLOADS {
        let a = cheapest_pass(w, Arch::CachedNonStream)
            .ok_or_else(|| format!("{}: no cached-nonstream model passes", w.name))?;
        let b = simulate(a.model, w, Arch::Streaming);
        if b.cost_day <= a.out.cost_day {
            return Err(format!("{}: streaming not more expensive — sim bug", w.name));
        }
        if a.out.staleness_s > w.freshness_need_s {
            return Err(format!("{}: cached staleness violates freshness need", w.name));
        }
        if a.out.accuracy < req_accuracy(w) {
            return Err(format!("{}: cached recommendation below threshold", w.name));
        }
    }
    // the cliff must actually require bigger models somewhere
    let t1 = cheapest_pass(&WORKLOADS[0], Arch::CachedNonStream).unwrap().model.tier;
    let t5 = cheapest_pass(&WORKLOADS[4], Arch::CachedNonStream).unwrap().model.tier;
    if t5 <= t1 {
        return Err("no tier escalation between single and 12-core".into());
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--md") => print!("{}", build_report(true)),
        Some("--check") => match check() {
            Ok(()) => println!("pv_bench check: OK — baseline invariants hold"),
            Err(e) => {
                eprintln!("pv_bench check: FAILED — {e}");
                std::process::exit(1);
            }
        },
        Some("--help") | Some("-h") => {
            println!("pv_bench [--md | --check]  — simulated Groq model CBA (no real CPU/API use)");
        }
        _ => print!("{}", build_report(false)),
    }
}
