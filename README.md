# pv — Pressure Valve

**Intelligent process lifecycle management.** pv treats a process not as a PID
that is either running or dead, but as an *intent* with lifecycle semantics:
it understands what each process is trying to accomplish, predicts resource
exhaustion before it happens, and can suspend, resume, detach, and migrate
work accordingly.

```
Traditional                     Pressure Valve

fork()                          Intent
   │                               │
running                       Analyse → Execute → Monitor
   │                               │
kill                          Adapt → Suspend → Migrate → Resume → Retire
```

The process is no longer sacred. The objective is.

## Build

```sh
cargo build --release
# binary: target/release/pv
```

No services, no daemon, no root. Everything is read from `/proc`, `/sys`, and
kernel PSI (`/proc/pressure/*`) at invocation time.

## Commands

### `pv` — dashboard

Pressure bars (CPU / RAM / IO / battery / thermals), the contextual process
view, and concrete recommendations.

![pv dashboard, intent recognition, and explain](docs/dashboard.gif)

```
Pressure
  CPU       ░░░░░░░░  load 0.93 on 12 cores, psi 0%
  RAM       ███░░░░░  4.8/7.4 GB used (66%), swap 4.0 GB in use, psi 0%
  ...

Processes
  Chrome             Web browsing       idle      3.2 GB   safe to suspend 80%
  Containerd         Container runtime  idle      44 MB    never suspend

Recommendations
  [suspend] Suspend Chrome (+3.1 GB) (80%)
```

### `pv ps` — contextual ps

Every app with its inferred intent and lifecycle capabilities
(`suspend · interrupt · migrate`).

### `pv pressure` / `pv explain`

Detailed pressure breakdown with memory burn rate, and a plain-language
explanation of the system state including projected OOM ETA when memory is
genuinely draining.

### `pv intent <cmd...>` — intent recognition

Classify a command without running it:

```
$ pv intent cargo build --release
Task: Build Rust workspace
  Can survive interrupt: yes
  Can migrate:           yes
  Remote friendly:       yes
```

### `pv run -- <cmd...>` — continuity

Runs the command detached (setsid, output to a log). SSH can drop, the
terminal can close — the session survives. `pv sessions` lists them,
`pv attach <id>` follows the output again.

### `pv suspend` / `pv resume` / `pv kill`

Graceful suspension: syncs filesystem buffers, freezes the whole app with
SIGSTOP, records what it holds. Protected intents (ssh sessions, container
runtimes, package operations, databases) are refused unless `--force`.

```
$ pv suspend firefox
❄ Firefox frozen — 1.3 GB held, resume with `pv resume firefox`
```

![pv suspend and resume of a live process](docs/suspend.gif)

### `pv policy` — pressure policies

Declarative rules in `~/.config/pv/policies.toml` (`pv policy --init` for
defaults), evaluated against live state. Dry-run by default, `--apply` to act
(suspend / throttle via renice+ionice / notify).

```toml
[[rule]]
name = "idle-heavy-browser"
category = "browser"
min_rss_mb = 300
min_mem_pressure = 40
action = "suspend"
message = "Suspend {app} (+{rss_mb} MB)"
```

### `pv live` — dynamic mode

A persistent, realtime view: gauges, the app table with RSS trend arrows
(▲/▼), frozen apps, and a **streaming Groq inference panel** that narrates
what matters, what happens next, and the one action to take.

```
╔═[ PV::LIVE ]═[ 15:55:05 ]═[ 1s ]════════════════════════════╗
║ CPU ██░░░░░░ 36%  RAM ███░░░░░ 48%  IO █░░░░░░░ 21%         ║
║ LOAD 11.23/12  MEM -78.1MB/s  OOM~31s  THERM 77°C  BAT 56%▼ ║
╟─[ processes ]───────────────────────────────────────────────╢
║ Chrome           browser    idle      2.7 GB ▼              ║
║ Rust-lld         unknown    13%       2.5 GB ▲              ║
║ Rustc            build      120%      1.3 GB ▼              ║
╟─[ groq :: llama-3.1-8b-instant ]────────────────────────────╢
║ RAM usage is high at 63%. Chrome is consuming 47% of CPU,   ║
║ likely causing high load. Shut down Chrome to free resources║
║ infer: 15:55:00 · 32 tok                                    ║
╚═[ q quit ]══════════════════════════════════════════════════╝
```

![pv live — realtime gauges and process trends](docs/live.gif)

```sh
pv live                      # 1s redraw, streaming inference
pv live --no-infer           # metrics only
pv live --model llama-3.3-70b-versatile   # sharper, slower/costlier
```

Inference is served by Groq's OpenAI-compatible streaming API, reached over
`curl` (no HTTP/TLS dependencies). Key resolution: `$GROQ_API_KEY`, then
`~/.config/pv/groq_api_key`. Without a key the panel degrades to an offline
notice and the metrics stay live. Inference re-fires only when the
*meaningful* state changes (pressure bands, top apps, OOM ETA, battery) or
on a 120 s heartbeat — never on a fixed hot loop. The default model,
`llama-3.1-8b-instant`, is fast and cheap but editorializes; the 70B model
follows the snapshot much more faithfully.

### `pv update` — self-update

```sh
pv update            # latest GitHub release binary → ~/.local/bin/pv
pv update --source   # clone main, cargo build --release, install
pv update --system   # install to /usr/local/bin via sudo
pv update --check    # report versions only, change nothing
pv update --force    # reinstall even when current == latest
```

Resolution order: latest release asset from GitHub (public API, no auth);
when the repo has no releases yet, it falls back to a source build
(`git clone --depth 1` → `cargo build --release` → install). The install
target is `~/.local/bin` by default — or `/usr/local/bin` when `--system` is
given or pv already runs from there. The swap is atomic (rename), so
updating a running pv is safe. Releases ship a single asset named
`pv-linux-x86_64`.

### `pv daemon` / `pv notify` — the valve board

`pv notify` emits one-shot valve cards for the current state; `pv daemon`
watches continuously. The daemon learns your **habits** (per-hour demand
profile, persisted to `~/.local/share/pv/habits.json`), tracks **variance**
in resource demand over a sliding window, and vents notifications when
current or *likely* bottlenecks form — each as a rustic valve card with one
concrete action:

```
   )   (   )   (   )   (   )   (   )   (   )   (
  (   )   (   )   (  STEAM OVERPRESSURE  )   (   )
╔═[ PV::VALVE ]═[ V-01 CRITICAL ]════════════════════════╗
║ 09:54:57 · local watch · vent 0x4AE1                   ║
║                                                        ║
║ MEMORY OVERPRESSURE                                    ║
║ RAM ▰▰▰▰▰▰▰▰▱▱ 81%                                     ║
║                                                        ║
║ 81% of physical memory committed                       ║
║ projected exhaustion in 1m 02s                         ║
╚═[ end of line ]════════════════════════════════════════╝

╔═[ PV::VALVE ]═[ V-06 ADVISORY ]════════════════════════╗
║ 10:02:11 · local watch · vent 0x51C3                   ║
║                                                        ║
║ DEMAND FRONT INCOMING                                  ║
║ RAM ▰▰▰▰▰▰▰▱▱▱ 74%                                     ║
║                                                        ║
║ your 11:00 block usually runs ~74% RAM (now 52%)       ║
║ usual suspects: build, browser                         ║
║                                                        ║
║ › run heavy jobs now, or plan a migration              ║
╚═[ end of line ]════════════════════════════════════════╝
```

Eight valves are armed:

| Valve | Level | Fires when |
|-------|-------|-----------|
| V-01 | warning/critical | RAM sustained-climbing past 72%, or ≥85% committed / OOM ETA < 10 min |
| V-02 | advisory | current load is ≥1.8× your learned baseline for this hour |
| V-03 | advisory | a heavy app is idle and reclaimable under memory pressure |
| V-04 | warning | swap keeps growing while memory PSI shows real stalls |
| V-05 | advisory | load variance is erratic (bursty demand thrashes the system) |
| V-06 | advisory | the next hour historically runs much heavier than now |
| V-07 | warning/critical | battery discharging below 15% / 8% with migratable work running |
| V-08 | warning/critical | thermals past 82°C / 90°C |

Each valve has a cooldown (advisory 15 min, warning 10, critical 5);
escalation bypasses it. `--desktop` also bridges to `notify-send` when one
is installed. `pv habits` shows the learned profile:

```
→ 09:00  cpu  16% █░░░░░░░  mem  71% █████░░░  var 0.04  build, browser
```

Run it under systemd:

```sh
pv daemon --install   # writes ~/.config/systemd/user/pv-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now pv-daemon
```

### `pv hosts` / `pv migrate` — device migration

Configure remote machines in `~/.config/pv/hosts.toml`:

```toml
[hosts.desktop]
addr = "sal@192.168.1.20"
note = "16 cores, 64 GB, RTX 5090"
```

`pv migrate <app-or-session> --to desktop` moves *restartable* intents
(builds, encodes): it rsyncs the working directory across and resumes the
command remotely, streaming output back. Intent-aware — it refuses to
"migrate" a database or your editor.

## How it decides

- **Intent recognition** (`src/intent.rs`) maps executables and command lines
  to categories — build, browser, encode, LLM, shell, backup, … — each with
  interrupt/suspend/migrate semantics.
- **Pressure** (`src/pressure.rs`) blends kernel PSI stall averages, load vs
  cores, `MemAvailable` trend sampling (OOM ETA), battery, and thermals.
- **Idle/audio detection** uses sampled CPU% and open `/dev/snd` handles, so a
  browser playing music is not "idle".
- **Suspend confidence** is computed per app from intent, activity, audio,
  and TTY ownership.

## Roadmap

- CRIU-based checkpoint/restore for true state-preserving migration
- Application plugin API (`#[pv::managed]`) for cooperative checkpointing
- cgroup freezer backend as an alternative to SIGSTOP
- Policy auto-apply from the daemon loop (policies currently evaluate per invocation)

## Demos

The GIFs under `docs/` are recorded with
[VHS](https://github.com/charmbracelet/vhs) from the tapes in the same
directory. Re-render after UI changes with:

```sh
vhs docs/dashboard.tape
vhs docs/live.tape
vhs docs/suspend.tape
```

## Ecosystem

Part of the Vico Labs toolchain: **kaptaind** (repository state),
**locksmith** (workspace locking), **amber** (dependency graphs),
**marty** (repo traversal), **bound** (context assembly), **fract**
(architecture) — and **pv**, which manages the execution lifecycle of all of
them.
