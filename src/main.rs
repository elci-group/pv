//! pv — Pressure Valve: intelligent process lifecycle management.

mod commands;
mod daemon;
mod display;
mod groq;
mod intent;
mod live;
mod migrate;
mod notify;
mod policy;
mod pressure;
mod procfs;
mod recommend;
mod session;
mod suspend;
mod update;

use clap::{Parser, Subcommand};
use display::Theme;

const HELP_BANNER: &str = r#"
         .  .  .
        .  .  .  .
          _______
         /_______\
        |\ \   / /|
        | \ \ / / |
        |  \ V /  |
        |  /   \  |
        | /     \ |
        |/_______\|
          |  |  |
       ___|  |  |___
      /             \
   __|               |__
  |___|             |___|

  P R E S S U R E   V A L V E — intelligent process lifecycle management

  A process is no longer just running or dead. It is an intent with a
  lifecycle: analysed, monitored, suspended, migrated, resumed, retired.

  Full manual: man -l docs/pv.1
"#;

#[derive(Parser)]
#[command(name = "pv", version, about = "Pressure Valve — intelligent process lifecycle management", long_about = HELP_BANNER)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Contextual process list (what is each process trying to do?)
    Ps,
    /// Detailed pressure breakdown (cpu, ram, io, battery, thermals)
    Pressure,
    /// Explain the current system state in plain language
    Explain,
    /// Classify a command without running it
    Intent {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Run a command as a detached pv session (survives disconnect)
    Run {
        /// Run on a configured remote host instead
        #[arg(long)]
        remote: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// List pv sessions
    Sessions,
    /// Follow a session's output
    Attach { id: String },
    /// Gracefully suspend an app (by name or pid)
    Suspend {
        target: String,
        /// Override never-suspend protections
        #[arg(long)]
        force: bool,
    },
    /// Resume a suspended app
    Resume { target: String },
    /// Terminate a suspended app (thaws, then SIGTERM)
    Kill { target: String },
    /// List suspended apps
    Suspended,
    /// Evaluate pressure policies (--apply to act, --init to write defaults)
    Policy {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        init: bool,
    },
    /// List configured remote hosts
    Hosts {
        #[arg(long)]
        init: bool,
    },
    /// Migrate restartable work to a remote host
    Migrate {
        target: String,
        #[arg(long)]
        to: Option<String>,
    },
    /// Run the observation daemon: learns habits, vents valve notifications
    Daemon {
        /// Seconds between observations
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// Also fire desktop notifications via notify-send
        #[arg(long)]
        desktop: bool,
        /// Write a systemd --user unit and print enable instructions
        #[arg(long)]
        install: bool,
    },
    /// Emit valve notification cards for the current state, then exit
    Notify {
        /// Also fire desktop notifications via notify-send
        #[arg(long)]
        desktop: bool,
    },
    /// Show the demand profile the daemon has learned about you
    Habits,
    /// Persistent dynamic view: realtime metrics + streaming Groq inference
    Live {
        /// Seconds between redraws
        #[arg(long, default_value_t = 1)]
        interval: u64,
        /// Groq model to stream from
        #[arg(long, default_value_t = groq::DEFAULT_MODEL.to_string())]
        model: String,
        /// Disable inference (metrics only)
        #[arg(long)]
        no_infer: bool,
    },
    /// Update pv: latest GitHub release binary, or clone+build from source
    Update {
        /// Force source build instead of release download
        #[arg(long)]
        source: bool,
        /// Install to /usr/local/bin (via sudo) instead of ~/.local/bin
        #[arg(long)]
        system: bool,
        /// Reinstall even when already on the latest version
        #[arg(long)]
        force: bool,
        /// Only check versions, do not download or install
        #[arg(long)]
        check: bool,
        /// GitHub repo to update from
        #[arg(long, default_value_t = update::DEFAULT_REPO.to_string())]
        repo: String,
    },
}

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

fn main() {
    // behave like a normal Unix tool when the reader goes away (pv ps | head)
    unsafe {
        signal(13 /*SIGPIPE*/, 0 /*SIG_DFL*/)
    };
    let cli = Cli::parse();
    let theme = Theme::new();
    suspend::gc();
    session::gc();

    use commands as c;
    let code = match cli.cmd {
        None => c::dashboard(&theme),
        Some(Cmd::Ps) => c::ps(&theme),
        Some(Cmd::Pressure) => c::pressure(&theme),
        Some(Cmd::Explain) => c::explain(&theme),
        Some(Cmd::Intent { cmd }) => c::intent(&theme, &cmd),
        Some(Cmd::Run { remote, cmd }) => c::run(&theme, &cmd, remote),
        Some(Cmd::Sessions) => c::sessions(&theme),
        Some(Cmd::Attach { id }) => c::attach(&theme, &id),
        Some(Cmd::Suspend { target, force }) => c::suspend(&theme, &target, force),
        Some(Cmd::Resume { target }) => c::resume(&theme, &target),
        Some(Cmd::Kill { target }) => c::kill(&theme, &target),
        Some(Cmd::Suspended) => c::suspended(&theme),
        Some(Cmd::Policy { apply, init }) => c::policy(&theme, apply, init),
        Some(Cmd::Hosts { init }) => c::hosts(&theme, init),
        Some(Cmd::Migrate { target, to }) => c::migrate(&theme, &target, to),
        Some(Cmd::Daemon {
            interval,
            desktop,
            install,
        }) => {
            if install {
                daemon::install_service()
            } else {
                daemon::run_daemon(&theme, interval, desktop)
            }
        }
        Some(Cmd::Notify { desktop }) => daemon::run_notify(&theme, desktop),
        Some(Cmd::Habits) => daemon::print_habits(&theme),
        Some(Cmd::Live {
            interval,
            model,
            no_infer,
        }) => live::run_live(&theme, interval, &model, no_infer),
        Some(Cmd::Update {
            source,
            system,
            force,
            check,
            repo,
        }) => update::run(
            &theme,
            &update::Options {
                source,
                system,
                force,
                check,
                repo,
            },
        ),
    };
    std::process::exit(code);
}
