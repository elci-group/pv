//! Intent recognition: what is a process trying to accomplish, and what
//! lifecycle operations does that intent tolerate?

use crate::procfs::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Build,
    Browser,
    Editor,
    Encode,
    Llm,
    Shell,
    Container,
    Backup,
    Download,
    Database,
    System,
    Desktop,
    App,
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Intent {
    pub category: Category,
    pub task: String,          // "Build Rust workspace"
    pub interactive: bool,     // a human is (or may be) driving it
    pub can_suspend: bool,     // safe to freeze without corruption
    pub can_interrupt: bool,   // can be killed & restarted without losing much
    pub can_migrate: bool,     // restartable on another host (state-free or checkpointable)
    pub remote_friendly: bool, // worth running remotely
    pub gpu: bool,
    pub never_suspend: bool, // hard rule: interactive sessions, runtimes
    pub detail: String,      // extra context line, e.g. "Watching YouTube"
}

impl Intent {
    fn base(category: Category, task: &str) -> Self {
        Intent {
            category,
            task: task.to_string(),
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
}

/// Classify an app by its executable key + representative cmdline.
pub fn classify(app: &App) -> Intent {
    classify_cmd(&app.key, &app.cmdline, app.tty)
}

/// Classify an arbitrary command line (used by `pv intent` and `pv run`).
pub fn classify_command(cmdline: &str) -> Intent {
    let key = cmdline
        .split_whitespace()
        .next()
        .map(|c| {
            std::path::Path::new(c)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    classify_cmd(&key, cmdline, true)
}

fn classify_cmd(key: &str, cmd: &str, tty: bool) -> Intent {
    let k = key.trim_start_matches(|c: char| !c.is_alphanumeric());
    let cl = cmd.to_lowercase();

    // ----- builds -----
    if matches!(k, "cargo" | "rustc" | "sccache")
        || (cl.contains("cargo ") && matches!(k, "rustup" | "cargo"))
    {
        let mut i = Intent::base(Category::Build, "Build Rust workspace");
        i.can_interrupt = true;
        i.can_migrate = true;
        i.remote_friendly = true;
        i.detail = "survives interruption; incremental artifacts persist".into();
        return i;
    }
    if matches!(
        k,
        "make"
            | "ninja"
            | "cmake"
            | "gcc"
            | "g++"
            | "clang"
            | "cc"
            | "go"
            | "javac"
            | "mvn"
            | "gradle"
            | "pnpm"
            | "npm"
            | "yarn"
            | "tsc"
            | "webpack"
            | "vite"
            | "bun"
    ) {
        let mut i = Intent::base(
            Category::Build,
            match k {
                "go" => "Build Go workspace",
                "javac" | "mvn" | "gradle" => "Build JVM project",
                "npm" | "yarn" | "pnpm" | "bun" => "JS toolchain task",
                "vite" | "webpack" | "tsc" => "JS bundle / typecheck",
                _ => "Compile project",
            },
        );
        i.can_interrupt = true;
        i.can_migrate = true;
        i.remote_friendly = true;
        if k == "vite" || (k == "npm" && (cl.contains(" dev") || cl.contains(" start"))) {
            i.task = "JS dev server".into();
            i.interactive = true;
            i.can_migrate = false;
            i.remote_friendly = false;
        }
        return i;
    }

    // ----- browsers -----
    if matches!(
        k,
        "firefox"
            | "chromium"
            | "chrome"
            | "google-chrome"
            | "brave"
            | "brave-browser"
            | "vivaldi"
            | "opera"
            | "librewolf"
            | "zen"
            | "zen-browser"
    ) || k.contains("chrom")
    {
        let mut i = Intent::base(Category::Browser, "Web browsing");
        i.interactive = true;
        i.can_suspend = true;
        i.detail = "safe to suspend when idle; tabs restore on resume".into();
        return i;
    }

    // ----- encoders / media -----
    if matches!(
        k,
        "ffmpeg"
            | "handbrake"
            | "handbrakecli"
            | "x264"
            | "x265"
            | "avifenc"
            | "cwebp"
            | "convert"
            | "magick"
    ) {
        let mut i = Intent::base(Category::Encode, "Media encode / transcode");
        i.can_interrupt = true;
        i.can_migrate = true;
        i.remote_friendly = true;
        i.gpu = cl.contains("nvenc") || cl.contains("vaapi") || cl.contains("qsv");
        i.detail = "restartable; strong remote/GPU candidate".into();
        return i;
    }

    // ----- LLM / inference -----
    if matches!(
        k,
        "ollama" | "llama-server" | "llama.cpp" | "vllm" | "text-generation-webui" | "koboldcpp"
    ) || cl.contains("llama")
        || (k == "python" || k == "python3") && (cl.contains("vllm") || cl.contains("transformers"))
    {
        let mut i = Intent::base(Category::Llm, "LLM inference");
        i.gpu = true;
        i.never_suspend = true;
        i.detail = "reserve VRAM; suspension breaks in-flight requests".into();
        return i;
    }

    // ----- interactive shells & remote sessions -----
    if matches!(k, "ssh" | "mosh" | "telnet") {
        let mut i = Intent::base(Category::Shell, "Interactive remote session");
        i.interactive = true;
        i.never_suspend = true;
        i.detail = "never suspend: live session".into();
        return i;
    }
    if matches!(
        k,
        "bash" | "zsh" | "fish" | "sh" | "tmux" | "screen" | "nu" | "xonsh"
    ) {
        let mut i = Intent::base(Category::Shell, "Interactive shell");
        i.interactive = tty;
        i.never_suspend = tty;
        return i;
    }

    // ----- containers / runtimes -----
    if matches!(
        k,
        "docker" | "dockerd" | "containerd" | "podman" | "buildkitd" | "runc" | "kubelet"
    ) {
        let mut i = Intent::base(Category::Container, "Container runtime");
        i.never_suspend = true;
        i.detail = "runtime: freezing breaks containers".into();
        return i;
    }

    // ----- backups / sync -----
    if matches!(
        k,
        "rsync" | "restic" | "borg" | "rclone" | "duplicity" | "tar" | "cp" | "mv" | "dd"
    ) {
        let mut i = Intent::base(Category::Backup, "Backup / bulk copy");
        i.can_interrupt = true;
        i.can_migrate = matches!(k, "rsync" | "rclone");
        i.detail = "throttleable; restartable".into();
        return i;
    }

    // ----- downloads -----
    if matches!(
        k,
        "curl" | "wget" | "aria2c" | "yt-dlp" | "torrent" | "transmission"
    ) {
        let mut i = Intent::base(Category::Download, "Download");
        i.can_interrupt = true;
        i.detail = "pausable; resumable".into();
        return i;
    }

    // ----- databases -----
    if matches!(
        k,
        "postgres"
            | "postmaster"
            | "mysqld"
            | "mariadbd"
            | "redis-server"
            | "mongod"
            | "clickhouse-server"
    ) {
        let mut i = Intent::base(Category::Database, "Database server");
        i.never_suspend = true;
        i.detail = "service: never suspend".into();
        return i;
    }

    // ----- editors -----
    if matches!(
        k,
        "code"
            | "codium"
            | "nvim"
            | "vim"
            | "emacs"
            | "sublime_text"
            | "zed"
            | "helix"
            | "hx"
            | "kate"
            | "gedit"
    ) {
        let mut i = Intent::base(Category::Editor, "Editing session");
        i.interactive = true;
        i.never_suspend = tty && matches!(k, "nvim" | "vim" | "emacs" | "hx" | "helix");
        i.detail = "user state lives here".into();
        return i;
    }

    // ----- package management -----
    if matches!(
        k,
        "apt"
            | "apt-get"
            | "dpkg"
            | "dnf"
            | "yum"
            | "pacman"
            | "zypper"
            | "nix"
            | "snap"
            | "flatpak"
    ) {
        let mut i = Intent::base(Category::App, "Package operation");
        i.never_suspend = true;
        i.detail = "interrupting mid-transaction can corrupt package state".into();
        return i;
    }

    // ----- system / desktop plumbing -----
    if matches!(
        k,
        "systemd"
            | "init"
            | "sshd"
            | "dbus-daemon"
            | "kwin"
            | "kwin_wayland"
            | "mutter"
            | "gnome-shell"
            | "plasmashell"
            | "sway"
            | "wayfire"
            | "xorg"
            | "xwayland"
            | "pipewire"
            | "wireplumber"
            | "pulseaudio"
            | "networkmanager"
            | "cupsd"
            | "polkitd"
            | "upowerd"
            | "xdg-desktop-portal"
            | "sddm"
            | "gdm"
            | "login"
            | "tailscaled"
            | "cloudflared"
            | "coredns"
            | "resolved"
            | "systemd-resolved"
            | "chronyd"
            | "ntpd"
            | "fwupd"
            | "udisksd"
            | "accounts-daemon"
            | "rtkit-daemon"
    ) || k.starts_with("cosmic-")
        || k.starts_with("plasma-")
        || k.starts_with("gnome-")
        || k.starts_with("xfce4-")
        || k.starts_with("systemd-")
    {
        let mut i = Intent::base(Category::System, "System service");
        i.never_suspend = true;
        i.can_suspend = false;
        return i;
    }
    if matches!(
        k,
        "dolphin"
            | "nautilus"
            | "thunar"
            | "konsole"
            | "gnome-terminal"
            | "alacritty"
            | "kitty"
            | "wezterm"
            | "foot"
            | "yakuake"
    ) {
        let mut i = Intent::base(Category::Desktop, "Desktop component");
        i.interactive = true;
        i.never_suspend = true; // terminals hold shells
        return i;
    }

    // ----- python / node scripts -----
    if matches!(k, "python" | "python3" | "node" | "deno" | "ruby" | "perl") {
        let mut i = Intent::base(Category::App, "Script");
        if cl.contains("jupyter") {
            i.task = "Jupyter server".into();
            i.never_suspend = true;
        } else if cl.contains("train") || cl.contains("torch") {
            i.task = "ML training".into();
            i.gpu = true;
            i.can_migrate = true;
            i.remote_friendly = true;
        }
        i.interactive = tty;
        return i;
    }

    // ----- fallback -----
    let mut i = Intent::base(Category::Unknown, "Application");
    i.interactive = tty;
    i.never_suspend = false;
    i
}

/// Suspend-safety verdict with a 0..100 confidence, for display.
pub fn suspend_confidence(app: &App, intent: &Intent, idle_secs: u64) -> u8 {
    if intent.never_suspend || !intent.can_suspend {
        return 0;
    }
    let mut c = 50u32;
    if app.cpu_pct < 1.0 {
        c += 20;
    } else if app.cpu_pct > 20.0 {
        c = c.saturating_sub(30);
    }
    if app.has_audio {
        c = c.saturating_sub(40);
    }
    if idle_secs > 600 {
        c += 15;
    }
    if matches!(intent.category, Category::Browser) {
        c += 10;
    }
    if app.tty {
        c = c.saturating_sub(25);
    }
    c.min(98) as u8
}
