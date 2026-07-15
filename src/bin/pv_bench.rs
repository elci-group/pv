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

    let accuracy = accuracy_at(m, w.req_tier);

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
/// Selection reuses `cheapest_passing` — per-call cost ordering is identical
/// to per-day ordering within a fixed (workload, arch).
fn cheapest_pass<'a>(w: &Workload, arch: Arch) -> Option<Ranked<'a>> {
    let model = cheapest_passing(w.req_tier, w.ctx_in as f64, w.ctx_out as f64)?;
    let out = simulate(model, w, arch);
    out.pass.then_some(Ranked { model, out })
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

    // ---- measured temperature sweep ---------------------------------------------
    temp_section(&mut s, md);

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

    // ---- dynamic scaling handover ----------------------------------------------
    scaling_section(&mut s, md);

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
    let _ = writeln!(s, "  handover     : dynamic ladder 8b <-> 20b <-> 120b over the day; pays on");
    let _ = writeln!(s, "                 octo+ devices — quad and below stay static (see above)");
    let _ = writeln!(s, "  temperature  : 0.0 for all supervision calls (measured sweep; pv was 0.2)");
    let _ = writeln!(s, "  pv live note : pv's own narration panel is a single-core-class task —");
    let _ = writeln!(s, "                 llama-3.1-8b-instant stays the right default there; the");
    let _ = writeln!(s, "                 rows above govern *agentic workflow supervision*.");
    if md { let _ = writeln!(s, "```"); }

    s
}

// ---------------------------------------------------------------------------
// Dynamic scaling handover
// ---------------------------------------------------------------------------
//
// A device sized for peak load should not pay peak-model prices at idle.
// Demand is modelled as a 24 h activity curve (fraction of cores busy) plus a
// deterministic 3 h wobble, sampled in 5-minute ticks. Three policies:
//
//   static      — one model, sized for the device's peak (today's behaviour)
//   naive       — re-pick the cheapest passing rung every tick
//   hysteresis  — scale UP immediately on an accuracy-floor breach, scale
//                 DOWN only after the cheaper rung holds for 2 ticks, and
//                 never below the supervisor floor (fleet devices keep an
//                 orchestrator-competent model even at deep idle)

/// 5-minute supervision ticks per simulated day.
const TICKS_PER_DAY: usize = 288;
/// Deterministic intra-hour demand wobble — creates realistic threshold crossings.
const WOBBLE: f64 = 0.15;
/// Ticks a cheaper rung must hold before a scale-down handover commits.
const SCALE_DOWN_DWELL: usize = 2;

/// 24 h activity anchors (fraction of cores busy), one row per workload,
/// in WORKLOADS order. Pre-defined faux values.
const DAY: [[f64; 24]; 5] = [
    // single — laptop: morning burst, afternoon focus block
    [0.30, 0.20, 0.15, 0.10, 0.10, 0.15, 0.25, 0.40, 0.60, 0.80, 0.90, 1.00,
     0.70, 0.55, 0.75, 0.90, 0.95, 0.80, 0.60, 0.50, 0.45, 0.40, 0.35, 0.30],
    // dual
    [0.35, 0.25, 0.20, 0.15, 0.15, 0.20, 0.30, 0.45, 0.65, 0.85, 0.95, 1.00,
     0.65, 0.50, 0.80, 0.90, 1.00, 0.85, 0.65, 0.55, 0.50, 0.45, 0.40, 0.35],
    // quad — workstation
    [0.20, 0.15, 0.10, 0.10, 0.10, 0.15, 0.30, 0.50, 0.70, 0.90, 1.00, 0.95,
     0.60, 0.55, 0.85, 0.95, 1.00, 0.90, 0.70, 0.55, 0.45, 0.35, 0.30, 0.25],
    // octo — build server with CI bursts, some night jobs
    [0.40, 0.35, 0.30, 0.30, 0.35, 0.45, 0.55, 0.65, 0.80, 0.95, 1.00, 0.90,
     0.70, 0.60, 0.85, 1.00, 0.95, 0.80, 0.60, 0.55, 0.50, 0.50, 0.45, 0.40],
    // 12-core — heavy workstation, deep night idle
    [0.15, 0.10, 0.08, 0.08, 0.10, 0.15, 0.25, 0.45, 0.70, 0.90, 1.00, 0.95,
     0.65, 0.50, 0.80, 0.95, 1.00, 0.85, 0.60, 0.45, 0.35, 0.25, 0.20, 0.15],
];

fn activity(dev: usize, tick: usize) -> f64 {
    let wobble = 1.0 + WOBBLE * (2.0 * std::f64::consts::PI * tick as f64 / 36.0).sin();
    (DAY[dev][tick / 12] * wobble).clamp(0.02, 1.0)
}

/// Effective demand at activity fraction f: (req_tier, ctx_in, ctx_out, events/hr).
/// Requirement tier scales sublinearly — even a partial fleet needs coordination.
fn demand(w: &Workload, f: f64) -> (f64, f64, f64, f64) {
    let req = 1.0 + (w.req_tier - 1.0) * f.powf(0.7);
    let ctx_in = 1800.0 + (w.ctx_in as f64 - 1800.0) * f;
    let ctx_out = 220.0 + (w.ctx_out as f64 - 220.0) * f;
    let events = w.events_hr * (0.25 + 0.75 * f);
    (req, ctx_in, ctx_out, events)
}

/// Fleet devices keep an orchestrator-competent supervisor even at deep idle.
fn supervisor_floor(w: &Workload) -> &'static Model {
    let id = if w.agents >= 2 { "openai/gpt-oss-20b" } else { "llama-3.1-8b-instant" };
    MODELS.iter().find(|m| m.id == id).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Static,
    Naive,
    Hysteresis,
}

struct DaySim {
    cost: f64,
    handovers: usize,
    violations: usize,
}

fn day_sim(w: &Workload, dev: usize, policy: Policy) -> DaySim {
    let floor = supervisor_floor(w);
    let (req1, ci1, co1, _) = demand(w, 1.0);
    let statik = cheapest_passing(req1, ci1, co1).unwrap();

    // dynamic policies establish on the tick-0 pick (device boots at current
    // demand, not at peak); only static pays for the peak-sized model from t0
    let mut cur: &Model = match policy {
        Policy::Static => statik,
        _ => {
            let (req0, ci0, co0, _) = demand(w, activity(dev, 0));
            let b = cheapest_passing(req0, ci0, co0).unwrap();
            if b.tier < floor.tier { floor } else { b }
        }
    };
    let (mut cost, mut handovers, mut violations) = (0.0, 0, 0);
    let mut dwell = 0usize;

    for tick in 0..TICKS_PER_DAY {
        let f = activity(dev, tick);
        let (req, ci, co, events) = demand(w, f);
        let need = req_accuracy_tier(req);
        let mut best = cheapest_passing(req, ci, co).unwrap();
        if best.tier < floor.tier {
            best = floor;
        }
        let next = match policy {
            Policy::Static => statik,
            Policy::Naive => best,
            Policy::Hysteresis => {
                if accuracy_at(cur, req) < need || best.tier > cur.tier {
                    dwell = 0;
                    best // floor breached or bigger rung required: immediate
                } else if best.tier < cur.tier {
                    dwell += 1;
                    if dwell >= SCALE_DOWN_DWELL {
                        dwell = 0;
                        best
                    } else {
                        cur
                    }
                } else {
                    dwell = 0;
                    best
                }
            }
        };
        if next.id != cur.id {
            handovers += 1;
            cur = next;
        }
        if accuracy_at(cur, req) < need {
            violations += 1;
        }
        // arch A call model at this activity level, one 5-min tick
        let calls = events + HEARTBEATS_HR * (1.0 - (events / HEARTBEATS_HR).min(0.8));
        cost += calls / 12.0 * call_cost(cur, ci, co);
    }
    DaySim { cost, handovers, violations }
}

/// Cheapest-passing rungs over the activity range, as (from_f, to_f, model).
fn ladder_runs(w: &Workload) -> Vec<(f64, f64, &'static Model)> {
    let mut runs: Vec<(f64, f64, &'static Model)> = Vec::new();
    let mut start = 0.0;
    let mut prev: Option<&Model> = None;
    for i in 0..=100 {
        let f = i as f64 / 100.0;
        let (req, ci, co, _) = demand(w, f.max(0.02));
        let m = cheapest_passing(req, ci, co).unwrap();
        if let Some(p) = prev {
            if p.id != m.id {
                runs.push((start, (i - 1) as f64 / 100.0, p));
                start = f;
            }
        }
        prev = Some(m);
    }
    if let Some(p) = prev {
        runs.push((start, 1.0, p));
    }
    runs
}

// ---------------------------------------------------------------------------
// Measured temperature sweep
// ---------------------------------------------------------------------------

/// One row of the measured sweep: (pass, agree, tok/call) per TEMPS point.
struct TempRow {
    model: &'static str,
    cells: [(f64, f64, u32); 6],
}

/// bench/temp_probe.py, 630 real Groq API calls, 2026-07-15.
/// pass = fraction of 15 deterministically-graded supervisor probes correct;
/// agree = self-agreement across 5 identical reps.
/// kimi-k2-0905 excluded: gated off the developer tier (model_not_found).
const TEMPS: [f64; 6] = [0.0, 0.2, 0.5, 0.8, 1.0, 1.5];
const TEMP_SWEEP: &[TempRow] = &[
    TempRow { model: "llama-3.1-8b-instant", cells: [
        (0.67, 1.00, 11), (0.67, 0.80, 11), (0.47, 0.67, 11),
        (0.47, 0.60, 11), (0.53, 0.67, 11), (0.40, 0.67, 11)] },
    TempRow { model: "meta-llama/llama-4-scout-17b-16e-instruct", cells: [
        (1.00, 1.00, 10), (1.00, 1.00, 10), (1.00, 1.00, 10),
        (1.00, 1.00, 10), (1.00, 1.00, 10), (0.93, 0.93, 10)] },
    TempRow { model: "openai/gpt-oss-20b", cells: [
        (1.00, 1.00, 156), (1.00, 1.00, 128), (1.00, 1.00, 176),
        (1.00, 1.00, 175), (0.93, 0.93, 185), (1.00, 1.00, 183)] },
    TempRow { model: "qwen/qwen3-32b", cells: [
        (1.00, 1.00, 355), (0.80, 0.87, 352), (0.93, 0.93, 332),
        (0.93, 0.93, 317), (0.87, 0.87, 349), (0.87, 0.87, 413)] },
    TempRow { model: "qwen/qwen3.6-27b", cells: [
        (0.67, 1.00, 720), (0.67, 1.00, 690), (0.60, 0.93, 651),
        (0.67, 1.00, 583), (0.67, 1.00, 715), (0.27, 0.67, 500)] },
    TempRow { model: "llama-3.3-70b-versatile", cells: [
        (1.00, 1.00, 9), (1.00, 1.00, 9), (0.93, 0.93, 9),
        (0.93, 0.93, 9), (0.87, 0.87, 9), (0.93, 0.93, 9)] },
    TempRow { model: "openai/gpt-oss-120b", cells: [
        (1.00, 1.00, 107), (1.00, 1.00, 103), (1.00, 1.00, 114),
        (1.00, 1.00, 118), (1.00, 1.00, 118), (1.00, 1.00, 144)] },
];

/// First argmax of pass*agree — ties resolve to the lowest temperature.
fn ideal_idx(row: &TempRow) -> usize {
    let mut best = 0;
    for (i, &(pass, agree, _)) in row.cells.iter().enumerate() {
        if pass * agree > row.cells[best].0 * row.cells[best].1 {
            best = i;
        }
    }
    best
}

/// Highest temp where every point from 0.0 up still scores ≥ 90% of ideal.
fn safe_max_temp(row: &TempRow) -> f64 {
    let bi = ideal_idx(row);
    let ideal = row.cells[bi].0 * row.cells[bi].1;
    let mut safe = TEMPS[0];
    for (i, &(pass, agree, _)) in row.cells.iter().enumerate() {
        if pass * agree >= 0.9 * ideal {
            safe = TEMPS[i];
        } else {
            break;
        }
    }
    safe
}

// ---------------------------------------------------------------------------
// Temperature artifact cross-check
// ---------------------------------------------------------------------------
//
// TEMP_SWEEP is hand-copied from bench/temperature.json; --check recomputes
// pass/agree per (model, temp) from that artifact so the const cannot
// silently drift. Deliberately small scanner for the known temp_probe.py
// output shape ("cells" -> model -> temp -> probe -> {"pass", "answers"}) —
// it validates a known artifact, it is not a general JSON parser.

/// `needle`'s byte index in `hay` at or after `from`.
fn scan_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    hay.get(from..)?.find(needle).map(|i| from + i)
}

/// Integer right after the key at `at` (`"pass": 5` — colon/space separated).
fn scan_u64_after(text: &str, at: usize, key: &str) -> Option<u64> {
    let rest = text.get(at + key.len()..)?;
    let rest = rest.trim_start_matches(|c: char| c == ':' || c.is_ascii_whitespace());
    let len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 {
        return None;
    }
    rest.get(..len)?.parse().ok()
}

/// Quoted strings of the `"answers": [...]` array starting at `at`.
/// Answers are canonical tokens — they never contain quotes or brackets.
fn scan_answers(text: &str, at: usize) -> Option<Vec<String>> {
    let rest = text.get(at + "\"answers\"".len()..)?;
    let open = rest.find('[')?;
    let inner = rest.get(open + 1..)?;
    let inner = inner.get(..inner.find(']')?)?;
    Some(inner.split('"').skip(1).step_by(2).map(str::to_string).collect())
}

/// (pass fraction, agreement) of one (model, temp) cell: 3 probes in artifact
/// order, pass = sum of probe passes / total answers, agree = mean per-probe
/// mode share — mirrors temp_probe.py's --report.
fn measured_cell(ttext: &str) -> Option<(f64, f64)> {
    let mut passes = 0u64;
    let mut total = 0u64;
    let mut shares = Vec::new();
    let mut at = 0usize;
    for _ in 0..3 {
        at = scan_from(ttext, "\"pass\"", at)?;
        passes += scan_u64_after(ttext, at, "\"pass\"")?;
        at = scan_from(ttext, "\"answers\"", at)?;
        let answers = scan_answers(ttext, at)?;
        if answers.is_empty() {
            return None;
        }
        total += answers.len() as u64;
        let mut mode = 0usize;
        for a in &answers {
            mode = mode.max(answers.iter().filter(|b| *b == a).count());
        }
        shares.push(mode as f64 / answers.len() as f64);
        at += "\"answers\"".len();
    }
    if total == 0 {
        return None;
    }
    Some((passes as f64 / total as f64, shares.iter().sum::<f64>() / shares.len() as f64))
}

/// Recompute every TEMP_SWEEP cell from the measured artifact and require
/// agreement within 0.01 (the const rounds to 2 decimals).
fn temp_artifact_check() -> Result<(), String> {
    const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/bench/temperature.json");
    let text = std::fs::read_to_string(PATH)
        .map_err(|e| format!("temperature artifact {PATH} unreadable: {e}"))?;
    for row in TEMP_SWEEP {
        let mkey = format!("\"{}\"", row.model);
        let mstart = scan_from(&text, &mkey, 0).ok_or_else(|| {
            format!("temperature artifact: model {} missing from {PATH}", row.model)
        })?;
        // the model's section ends where the next model key begins
        let mend = TEMP_SWEEP
            .iter()
            .filter_map(|r| scan_from(&text, &format!("\"{}\"", r.model), mstart + mkey.len()))
            .min()
            .unwrap_or(text.len());
        let mtext = text.get(mstart..mend).ok_or("temperature artifact: bad slice")?;
        for (ti, temp) in TEMPS.iter().enumerate() {
            // Python str(float) == Rust {:?} here: "0.0", "0.2", "1.0", ...
            let tkey = format!("\"{temp:?}\":");
            let tstart = scan_from(mtext, &tkey, 0).ok_or_else(|| {
                format!("temperature artifact: {} @ {temp:?} missing", row.model)
            })?;
            let tend = TEMPS[ti + 1..]
                .iter()
                .filter_map(|t| scan_from(mtext, &format!("\"{t:?}\":"), tstart + tkey.len()))
                .min()
                .unwrap_or(mtext.len());
            let ttext = mtext.get(tstart..tend).ok_or("temperature artifact: bad slice")?;
            let (got_pass, got_agree) = measured_cell(ttext).ok_or_else(|| {
                format!("temperature artifact: {} @ {temp:?} unparsable", row.model)
            })?;
            let (want_pass, want_agree, _) = row.cells[ti];
            if (got_pass - want_pass).abs() > 0.01 {
                return Err(format!(
                    "{} @ {temp:?}: TEMP_SWEEP pass {want_pass:.2} != artifact {got_pass:.2} — refresh the const from bench/temperature.json",
                    row.model
                ));
            }
            if (got_agree - want_agree).abs() > 0.01 {
                return Err(format!(
                    "{} @ {temp:?}: TEMP_SWEEP agree {want_agree:.2} != artifact {got_agree:.2} — refresh the const from bench/temperature.json",
                    row.model
                ));
            }
        }
    }
    Ok(())
}

fn temp_section(s: &mut String, md: bool) {
    h1(s, "Inference temperature (measured — 630 real API calls, 2026-07-15)", md);
    let _ = writeln!(s, "3 deterministically-graded supervisor probes (JSON action emission, enum");
    let _ = writeln!(s, "action selection, capacity arithmetic) x 5 identical reps x 6 temperatures.");
    let _ = writeln!(s, "pass = correctness vs exact rules, agree = self-agreement across reps.");
    let _ = writeln!(s, "kimi-k2-0905 excluded: gated off the developer tier (model_not_found).");
    let _ = writeln!(s, "caveats: n=5 reps per cell gives pass a resolution of ~0.07; trials were");
    let _ = writeln!(s, "interleaved round-robin across temperatures; argmax selection overstates");
    let _ = writeln!(s, "certainty, so ties resolve toward the lower temperature.\n");

    if md {
        let _ = writeln!(s, "| model | 0.0 | 0.2 | 0.5 | 0.8 | 1.0 | 1.5 | ideal | safe ≤ | tok/call |");
        let _ = writeln!(s, "|-------|----:|----:|----:|----:|----:|----:|------:|-------:|---------:|");
    } else {
        let _ = writeln!(s, "  {:<40} {:>28} {:>6} {:>6} {:>6}",
            "model", "pass @ 0.0/0.2/0.5/0.8/1.0/1.5", "ideal", "safe<=", "tok");
    }
    for row in TEMP_SWEEP {
        let bi = ideal_idx(row);
        let passes: Vec<String> = row.cells.iter().map(|c| format!("{:.2}", c.0)).collect();
        if md {
            let _ = writeln!(s, "| `{}` | {} | **{:.1}** | {:.1} | {} |",
                row.model, passes.join(" | "), TEMPS[bi], safe_max_temp(row), row.cells[bi].2);
        } else {
            let _ = writeln!(s, "  {:<40} {:>28} {:>6.1} {:>6.1} {:>6}",
                row.model, passes.join(" "), TEMPS[bi], safe_max_temp(row), row.cells[bi].2);
        }
    }

    h2(s, "Findings", md);
    code(s, "1. temp 0.0 is ideal for EVERY measured model — no model gained\n\
             \x20   anything from temperature; agreement is the first casualty\n\
2. pv's old 0.2 default was harmless for strong models but already cost\n\
             \x20   8b-instant 20% agreement — pv now calls at temperature 0.0\n\
3. high temp inflates reasoning tokens (gpt-oss-120b +35%, qwen3-32b +16%\n\
             \x20   at 1.5) — slower and costlier for worse answers\n\
4. qwen3.6-27b cannot emit single-JSON at ANY temp (parse-fail 5/5 across\n\
             \x20   the board) and collapses at 1.5 — unsuitable for structured\n\
             \x20   supervision output regardless of temperature\n\
5. 8b-instant's cap is capability, not temperature (arith 0/5 at every\n\
             \x20   temp) — temperature cannot fix a weak model, it can only\n\
             \x20   break a strong one\n\
6. per-model safe ceilings: 120b eats anything (<=1.5); scout <=1.0;\n\
             \x20   gpt-oss-20b <=0.8; 70b <=0.2; 8b, qwen3-32b, qwen3.6\n\
             \x20   need 0.0 (or near it) to stay reliable", md);
    let _ = writeln!(s);
}

fn nick(m: &Model) -> &'static str {
    match m.id {
        "llama-3.1-8b-instant" => "8b-instant",
        "meta-llama/llama-4-scout-17b-16e-instruct" => "l4-scout",
        "openai/gpt-oss-20b" => "gpt-oss-20b",
        "qwen/qwen3-32b" => "qwen3-32b",
        "qwen/qwen3.6-27b" => "qwen3.6-27b",
        "llama-3.3-70b-versatile" => "70b-versatile",
        "openai/gpt-oss-120b" => "gpt-oss-120b",
        "moonshotai/kimi-k2-instruct-0905" => "kimi-k2",
        other => other,
    }
}

fn scaling_section(s: &mut String, md: bool) {
    h1(s, "Dynamic scaling handover — cheaper models when cores idle", md);
    let _ = writeln!(s, "24 h synthetic activity curves (anchors + {:.0}% 3 h-wobble), 5-min ticks,",
        WOBBLE * 100.0);
    let _ = writeln!(s, "arch-A call model; demand interpolated between single-core and device peak.\n");

    h2(s, "Model ladder per device (handover thresholds)", md);
    if md {
        let _ = writeln!(s, "| device | peak req tier | ladder over activity f |");
        let _ = writeln!(s, "|--------|--------------:|------------------------|");
    }
    for w in WORKLOADS {
        let runs = ladder_runs(w);
        let mut parts: Vec<String> = Vec::new();
        for (i, (from, to, m)) in runs.iter().enumerate() {
            if i + 1 == runs.len() && *from > 0.0 {
                parts.push(format!("{} f>{:.2}", nick(m), from));
            } else if runs.len() == 1 {
                parts.push(format!("{} (always)", nick(m)));
            } else {
                parts.push(format!("{} f≤{:.2}", nick(m), to));
            }
        }
        if md {
            let _ = writeln!(s, "| {} | {:.1} | {} |", w.name, w.req_tier, parts.join(" → "));
        } else {
            let _ = writeln!(s, "  {:<12} peak req {:.1}   {}", w.name, w.req_tier, parts.join(" → "));
        }
    }
    let _ = writeln!(s);

    h2(s, "Simulated day — static vs naive vs hysteresis", md);
    if md {
        let _ = writeln!(s, "| device | static $/day | naive $/day (handovers) | hysteresis $/day (handovers) | saved | floor violations |");
        let _ = writeln!(s, "|--------|-------------:|------------------------:|-----------------------------:|------:|-----------------:|");
    } else {
        let _ = writeln!(s, "  {:<12} {:>9} {:>16} {:>20} {:>7} {:>8}",
            "device", "static$", "naive$ (hd)", "hyst$ (hd)", "saved", "viols");
    }
    let (mut sum_static, mut sum_hyst) = (0.0, 0.0);
    for (i, w) in WORKLOADS.iter().enumerate() {
        let st = day_sim(w, i, Policy::Static);
        let na = day_sim(w, i, Policy::Naive);
        let hy = day_sim(w, i, Policy::Hysteresis);
        sum_static += st.cost;
        sum_hyst += hy.cost;
        let saved = (1.0 - hy.cost / st.cost) * 100.0;
        if md {
            let _ = writeln!(s, "| {} | {} | {} ({}) | {} ({}) | {:.0}% | {} |",
                w.name, money(st.cost), money(na.cost), na.handovers,
                money(hy.cost), hy.handovers, saved, hy.violations);
        } else {
            let _ = writeln!(s, "  {:<12} {:>9} {:>16} {:>20} {:>6.0}% {:>8}",
                w.name, money(st.cost),
                format!("{} ({})", money(na.cost), na.handovers),
                format!("{} ({})", money(hy.cost), hy.handovers),
                saved, hy.violations);
        }
    }
    let _ = writeln!(s, "\n  total: static {}/day → hysteresis {}/day ({:.0}% saved)",
        money(sum_static), money(sum_hyst), (1.0 - sum_hyst / sum_static) * 100.0);

    h2(s, "Handover strategy", md);
    code(s, "1. rungs        8b-instant (single-agent idle) -> gpt-oss-20b (fleet\n\
             \x20               floor) -> gpt-oss-120b (fleet peak); kimi-k2 premium only\n\
2. scale UP     the tick the accuracy floor would breach — under-tier\n\
             \x20   advice is worse than none; never wait\n\
3. scale DOWN   only after the cheaper rung holds 2 ticks (10 min) —\n\
             \x20   kills wobble flapping around a threshold\n\
4. floor        devices running an orchestrator never drop below\n\
             \x20   gpt-oss-20b, even at deep idle\n\
5. free handover supervision calls are stateless — the previous model's\n\
             \x20   last advice stays displayed until the new model's first\n\
             \x20   response lands (atomic swap, no flicker)\n\
6. degrade UP   if a handover call fails, keep the larger model\n\
7. pre-arm      pv habits knows the per-hour profile — when the next\n\
             \x20   hour historically runs >=2x current demand, warm the\n\
             \x20   bigger rung one tick early\n\
8. where it pays octo+ devices (peak req tier >= 3.5); octo gains most —\n\
             \x20   on 12-core the peak hours dominate spend, so savings are\n\
             \x20   real but smaller. quad and below already bottom out on\n\
             \x20   the cheap rung — stay static", md);
    let _ = writeln!(s);
}

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
    // dynamic handover: never worse than static, never flaps more than naive,
    // never breaches the accuracy floor
    for (i, w) in WORKLOADS.iter().enumerate() {
        let st = day_sim(w, i, Policy::Static);
        let na = day_sim(w, i, Policy::Naive);
        let hy = day_sim(w, i, Policy::Hysteresis);
        if hy.cost > st.cost {
            return Err(format!("{}: hysteresis costs more than static", w.name));
        }
        if hy.violations > 0 || na.violations > 0 {
            return Err(format!("{}: accuracy floor breached", w.name));
        }
        if hy.handovers > na.handovers {
            return Err(format!("{}: hysteresis flaps more than naive", w.name));
        }
    }
    // and it must actually pay where the strategy claims: octo and 12-core
    // each save something, and their combined saving is meaningful. (12-core
    // saves less than octo — its spend concentrates in the peak hours where
    // the big rung is genuinely required.)
    let (mut st_big, mut hy_big) = (0.0, 0.0);
    for i in [3usize, 4] {
        let st = day_sim(&WORKLOADS[i], i, Policy::Static);
        let hy = day_sim(&WORKLOADS[i], i, Policy::Hysteresis);
        if hy.cost >= st.cost {
            return Err(format!("{}: handover saves nothing", WORKLOADS[i].name));
        }
        st_big += st.cost;
        hy_big += hy.cost;
    }
    if hy_big > st_big * 0.80 {
        return Err(format!(
            "octo+12-core: combined handover saving under 20% ({}/{})",
            money(hy_big),
            money(st_big)
        ));
    }
    // measured temperature sweep: determinism and direction sanity
    if TEMP_SWEEP.len() < 7 {
        return Err("temperature sweep covers under 7 models".into());
    }
    let mut strong = 0;
    for row in TEMP_SWEEP {
        if row.cells[0].1 < 0.9 {
            return Err(format!("{}: agreement at temp 0.0 below 0.9", row.model));
        }
        let bi = ideal_idx(row);
        if TEMPS[bi] > 0.2 {
            return Err(format!("{}: ideal temp above 0.2 — supervision claim broken", row.model));
        }
        let ideal = row.cells[bi].0 * row.cells[bi].1;
        let hot = row.cells[5].0 * row.cells[5].1;
        if ideal < hot {
            return Err(format!("{}: scores better at 1.5 than at ideal — sweep noise?", row.model));
        }
        if row.cells[bi].0 >= 0.9 {
            strong += 1;
        }
    }
    if strong < 5 {
        return Err("under 5 models pass >= 0.9 at their ideal temp".into());
    }
    // the hand-copied const must match the measured artifact it came from
    temp_artifact_check()?;
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
