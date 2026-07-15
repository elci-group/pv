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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::App;

    /// Table row: (key, cmdline, tty, category, flags) where flags are
    /// [interactive, can_suspend, can_interrupt, can_migrate, remote_friendly,
    /// never_suspend, gpu].
    type Row = (&'static str, &'static str, bool, Category, [bool; 7]);

    fn check(rows: &[Row]) {
        for (key, cmd, tty, category, f) in rows.iter().copied() {
            let i = classify_cmd(key, cmd, tty);
            let ctx = format!("key={key:?} cmd={cmd:?} tty={tty}");
            assert_eq!(i.category, category, "category: {ctx}");
            assert_eq!(i.interactive, f[0], "interactive: {ctx}");
            assert_eq!(i.can_suspend, f[1], "can_suspend: {ctx}");
            assert_eq!(i.can_interrupt, f[2], "can_interrupt: {ctx}");
            assert_eq!(i.can_migrate, f[3], "can_migrate: {ctx}");
            assert_eq!(i.remote_friendly, f[4], "remote_friendly: {ctx}");
            assert_eq!(i.never_suspend, f[5], "never_suspend: {ctx}");
            assert_eq!(i.gpu, f[6], "gpu: {ctx}");
        }
    }

    // interruptible, migratable, worth running remotely
    const BUILD: [bool; 7] = [false, true, true, true, true, false, false];
    // dev server variant: a human is driving, bound to this host
    const DEV_SERVER: [bool; 7] = [true, true, true, false, false, false, false];
    const BROWSER: [bool; 7] = [true, true, false, false, false, false, false];
    const ENCODE: [bool; 7] = [false, true, true, true, true, false, false];
    // never suspend; reserves the GPU
    const LLM: [bool; 7] = [false, true, false, false, false, true, true];
    // live session attached to a terminal
    const SESSION: [bool; 7] = [true, true, false, false, false, true, false];
    // untouchable plumbing: cannot even be frozen
    const SYSTEM: [bool; 7] = [false, false, false, false, false, true, false];

    #[test]
    fn builds_are_interruptible_and_migratable() {
        check(&[
            (
                "cargo",
                "cargo build --release",
                true,
                Category::Build,
                BUILD,
            ),
            ("cargo", "cargo test", false, Category::Build, BUILD),
            ("rustc", "rustc main.rs", true, Category::Build, BUILD),
            (
                "sccache",
                "sccache rustc lib.rs",
                false,
                Category::Build,
                BUILD,
            ),
            (
                "rustup",
                "rustup run stable cargo build",
                true,
                Category::Build,
                BUILD,
            ),
            ("make", "make -j8", true, Category::Build, BUILD),
            ("ninja", "ninja -C build", false, Category::Build, BUILD),
            ("cmake", "cmake --build build", true, Category::Build, BUILD),
            ("gcc", "gcc -O2 main.c", true, Category::Build, BUILD),
            ("go", "go build ./...", true, Category::Build, BUILD),
            ("javac", "javac Main.java", true, Category::Build, BUILD),
            ("mvn", "mvn package", true, Category::Build, BUILD),
            ("gradle", "gradle assemble", false, Category::Build, BUILD),
            ("npm", "npm run build", true, Category::Build, BUILD),
            ("pnpm", "pnpm build", false, Category::Build, BUILD),
            ("yarn", "yarn build", true, Category::Build, BUILD),
            ("tsc", "tsc --noEmit", true, Category::Build, BUILD),
            (
                "webpack",
                "webpack --mode production",
                true,
                Category::Build,
                BUILD,
            ),
            ("bun", "bun build index.ts", true, Category::Build, BUILD),
        ]);
    }

    #[test]
    fn npm_dev_server_is_interactive_and_not_migratable() {
        for cmd in ["npm run dev", "npm start"] {
            let i = classify_command(cmd);
            assert_eq!(i.category, Category::Build, "category: {cmd:?}");
            assert_eq!(i.task, "JS dev server", "task: {cmd:?}");
            assert!(i.interactive, "interactive: {cmd:?}");
            assert!(i.can_interrupt, "can_interrupt: {cmd:?}");
            assert!(!i.can_migrate, "can_migrate: {cmd:?}");
            assert!(!i.remote_friendly, "remote_friendly: {cmd:?}");
            assert!(!i.never_suspend, "never_suspend: {cmd:?}");
        }
        check(&[
            ("npm", "npm run dev", true, Category::Build, DEV_SERVER),
            (
                "vite",
                "vite --port 5173",
                true,
                Category::Build,
                DEV_SERVER,
            ),
        ]);
        // a plain npm build stays a batch job
        let i = classify_cmd("npm", "npm run build", true);
        assert!(!i.interactive && i.can_migrate && i.remote_friendly);
    }

    #[test]
    fn browsers_are_interactive_but_suspendable() {
        check(&[
            ("firefox", "firefox", true, Category::Browser, BROWSER),
            (
                "chromium",
                "chromium --incognito",
                true,
                Category::Browser,
                BROWSER,
            ),
            ("chrome", "chrome", true, Category::Browser, BROWSER),
            (
                "google-chrome",
                "google-chrome",
                false,
                Category::Browser,
                BROWSER,
            ),
            ("zen", "zen", true, Category::Browser, BROWSER),
            (
                "zen-browser",
                "zen-browser",
                true,
                Category::Browser,
                BROWSER,
            ),
            ("librewolf", "librewolf", true, Category::Browser, BROWSER),
        ]);
    }

    #[test]
    fn encoders_are_restartable_remote_candidates() {
        check(&[
            (
                "ffmpeg",
                "ffmpeg -i in.mkv -c:v libx264 out.mp4",
                true,
                Category::Encode,
                ENCODE,
            ),
            ("handbrake", "handbrake", true, Category::Encode, ENCODE),
            (
                "handbrakecli",
                "handbrakecli -i in -o out",
                false,
                Category::Encode,
                ENCODE,
            ),
            ("x265", "x265 in.yuv", false, Category::Encode, ENCODE),
            (
                "magick",
                "magick in.png out.webp",
                true,
                Category::Encode,
                ENCODE,
            ),
        ]);
    }

    #[test]
    fn encoders_detect_gpu_offload_flags() {
        for cmd in [
            "ffmpeg -i in.mp4 -c:v h264_nvenc out.mp4",
            "ffmpeg -hwaccel vaapi -i in.mp4 out.mp4",
            "ffmpeg -c:v h264_qsv -i in.mp4 out.mp4",
        ] {
            assert!(classify_cmd("ffmpeg", cmd, false).gpu, "gpu: {cmd:?}");
        }
        assert!(!classify_cmd("ffmpeg", "ffmpeg -i in.mp4 -c:v libx264 out.mp4", false).gpu);
    }

    #[test]
    fn llm_runtimes_are_gpu_and_never_suspend() {
        check(&[
            ("ollama", "ollama serve", true, Category::Llm, LLM),
            ("ollama", "ollama run llama3", false, Category::Llm, LLM),
            (
                "llama-server",
                "llama-server -m m.gguf",
                false,
                Category::Llm,
                LLM,
            ),
            ("koboldcpp", "koboldcpp m.gguf", true, Category::Llm, LLM),
            ("vllm", "vllm serve model", true, Category::Llm, LLM),
            (
                "text-generation-webui",
                "text-generation-webui",
                true,
                Category::Llm,
                LLM,
            ),
            (
                "python3",
                "python3 -m vllm.entrypoints.openai.api_server",
                false,
                Category::Llm,
                LLM,
            ),
            (
                "python",
                "python -c 'import transformers'",
                true,
                Category::Llm,
                LLM,
            ),
        ]);
        // a python script with no inference markers is not an LLM
        assert_eq!(
            classify_cmd("python3", "python3 script.py", false).category,
            Category::App
        );
    }

    #[test]
    fn ssh_and_shells_are_live_sessions() {
        check(&[
            (
                "ssh",
                "ssh user@example.com",
                true,
                Category::Shell,
                SESSION,
            ),
            ("mosh", "mosh user@host", true, Category::Shell, SESSION),
            ("telnet", "telnet host 23", true, Category::Shell, SESSION),
            ("bash", "bash", true, Category::Shell, SESSION),
            ("zsh", "zsh", true, Category::Shell, SESSION),
            ("tmux", "tmux attach", true, Category::Shell, SESSION),
            // login shells show up as "-bash" in argv[0]
            ("-bash", "-bash", true, Category::Shell, SESSION),
        ]);
        // without a tty a shell is just another batch process
        check(&[(
            "bash",
            "bash -c 'make'",
            false,
            Category::Shell,
            [false, true, false, false, false, false, false],
        )]);
    }

    #[test]
    fn system_and_desktop_daemons_are_untouchable() {
        check(&[
            ("systemd", "systemd", false, Category::System, SYSTEM),
            (
                "systemd-resolved",
                "systemd-resolved",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "systemd-logind",
                "systemd-logind",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "cosmic-comp",
                "cosmic-comp",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "cosmic-panel",
                "cosmic-panel",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "plasma-browser-integration-host",
                "plasma-browser-integration-host",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "plasmashell",
                "plasmashell",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "gnome-shell",
                "gnome-shell",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "gnome-software",
                "gnome-software",
                false,
                Category::System,
                SYSTEM,
            ),
            (
                "xfce4-panel",
                "xfce4-panel",
                false,
                Category::System,
                SYSTEM,
            ),
            ("pipewire", "pipewire", false, Category::System, SYSTEM),
            (
                "dbus-daemon",
                "dbus-daemon --system",
                false,
                Category::System,
                SYSTEM,
            ),
            ("sshd", "sshd -D", false, Category::System, SYSTEM),
        ]);
        // terminal emulators and file managers are interactive desktop parts
        check(&[
            ("konsole", "konsole", true, Category::Desktop, SESSION),
            ("kitty", "kitty", true, Category::Desktop, SESSION),
            ("nautilus", "nautilus", true, Category::Desktop, SESSION),
        ]);
    }

    #[test]
    fn remaining_categories_classify() {
        check(&[
            // containers: runtimes must not be frozen
            (
                "docker",
                "docker run nginx",
                true,
                Category::Container,
                [false, true, false, false, false, true, false],
            ),
            (
                "podman",
                "podman ps",
                false,
                Category::Container,
                [false, true, false, false, false, true, false],
            ),
            // backups: rsync/rclone can resume elsewhere, tar/dd cannot
            (
                "rsync",
                "rsync -av src/ dst/",
                true,
                Category::Backup,
                [false, true, true, true, false, false, false],
            ),
            (
                "rclone",
                "rclone copy a: b:",
                false,
                Category::Backup,
                [false, true, true, true, false, false, false],
            ),
            (
                "tar",
                "tar czf a.tgz dir",
                true,
                Category::Backup,
                [false, true, true, false, false, false, false],
            ),
            // downloads: resumable but host-bound
            (
                "curl",
                "curl -O https://example.com/f",
                true,
                Category::Download,
                [false, true, true, false, false, false, false],
            ),
            (
                "wget",
                "wget https://example.com/f",
                false,
                Category::Download,
                [false, true, true, false, false, false, false],
            ),
            // databases: services, never suspend
            (
                "postgres",
                "postgres -D /var/lib/pg",
                false,
                Category::Database,
                [false, true, false, false, false, true, false],
            ),
            (
                "redis-server",
                "redis-server",
                false,
                Category::Database,
                [false, true, false, false, false, true, false],
            ),
            // terminal editors hold user state only when on a tty
            (
                "nvim",
                "nvim file.rs",
                true,
                Category::Editor,
                [true, true, false, false, false, true, false],
            ),
            (
                "nvim",
                "nvim file.rs",
                false,
                Category::Editor,
                [true, true, false, false, false, false, false],
            ),
            (
                "code",
                "code .",
                true,
                Category::Editor,
                [true, true, false, false, false, false, false],
            ),
            // package transactions must run to completion
            (
                "apt",
                "apt install foo",
                true,
                Category::App,
                [false, true, false, false, false, true, false],
            ),
            (
                "pacman",
                "pacman -S foo",
                true,
                Category::App,
                [false, true, false, false, false, true, false],
            ),
            // scripts: plain, training, and jupyter flavors
            (
                "python3",
                "python3 script.py",
                true,
                Category::App,
                [true, true, false, false, false, false, false],
            ),
            (
                "python3",
                "python3 script.py",
                false,
                Category::App,
                [false, true, false, false, false, false, false],
            ),
            (
                "python3",
                "python3 train.py --epochs 10",
                false,
                Category::App,
                [false, true, false, true, true, false, true],
            ),
            (
                "python3",
                "python3 -m jupyter notebook",
                true,
                Category::App,
                [true, true, false, false, false, true, false],
            ),
            (
                "node",
                "node server.js",
                false,
                Category::App,
                [false, true, false, false, false, false, false],
            ),
        ]);
    }

    #[test]
    fn unknown_commands_get_a_sensible_default() {
        check(&[
            (
                "frobnicate",
                "frobnicate --fast",
                true,
                Category::Unknown,
                [true, true, false, false, false, false, false],
            ),
            (
                "frobnicate",
                "frobnicate --fast",
                false,
                Category::Unknown,
                [false, true, false, false, false, false, false],
            ),
        ]);
        let i = classify_cmd("frobnicate", "frobnicate --fast", true);
        assert_eq!(i.task, "Application");
        assert!(i.detail.is_empty());
    }

    #[test]
    fn task_strings_match_the_taxonomy() {
        let rows: &[(&str, &str, &str)] = &[
            ("cargo", "cargo build", "Build Rust workspace"),
            ("go", "go build ./...", "Build Go workspace"),
            ("javac", "javac Main.java", "Build JVM project"),
            ("npm", "npm run build", "JS toolchain task"),
            ("npm", "npm run dev", "JS dev server"),
            ("vite", "vite", "JS dev server"),
            ("tsc", "tsc -w", "JS bundle / typecheck"),
            ("make", "make", "Compile project"),
            ("firefox", "firefox", "Web browsing"),
            ("ffmpeg", "ffmpeg -i a b", "Media encode / transcode"),
            ("ollama", "ollama serve", "LLM inference"),
            ("ssh", "ssh host", "Interactive remote session"),
            ("bash", "bash", "Interactive shell"),
            ("docker", "docker ps", "Container runtime"),
            ("rsync", "rsync a b", "Backup / bulk copy"),
            ("curl", "curl url", "Download"),
            ("postgres", "postgres", "Database server"),
            ("nvim", "nvim", "Editing session"),
            ("apt", "apt update", "Package operation"),
            ("cosmic-comp", "cosmic-comp", "System service"),
            ("konsole", "konsole", "Desktop component"),
            ("python3", "python3 x.py", "Script"),
            ("python3", "python3 train.py", "ML training"),
            ("python3", "python3 -m jupyterlab", "Jupyter server"),
            ("frobnicate", "frobnicate", "Application"),
        ];
        for (key, cmd, task) in rows.iter().copied() {
            assert_eq!(classify_cmd(key, cmd, true).task, task, "task: {key:?}");
        }
    }

    #[test]
    fn classify_command_derives_the_key_from_the_cmdline() {
        // basename + lowercase of the first token; tty is assumed
        let i = classify_command("/usr/bin/firefox --new-window");
        assert_eq!(i.category, Category::Browser);
        assert!(i.interactive);

        let i = classify_command("CHROMIUM --headless");
        assert_eq!(i.category, Category::Browser);

        let i = classify_command("cargo build --release");
        assert_eq!(i.category, Category::Build);
        assert!(i.can_migrate);

        let i = classify_command("bash");
        assert_eq!(i.category, Category::Shell);
        assert!(i.interactive && i.never_suspend);

        assert_eq!(classify_command("").category, Category::Unknown);
    }

    fn app(key: &str, cmdline: &str) -> App {
        App {
            key: key.to_string(),
            display: key.to_string(),
            pids: vec![1],
            leader: 1,
            rss_kb: 0,
            cpu_pct: 0.0,
            state: 'S',
            tty: false,
            has_audio: false,
            cmdline: cmdline.to_string(),
            argv: vec![key.to_string()],
            age_secs: 0.0,
            kernel: false,
        }
    }

    #[test]
    fn classify_delegates_to_the_app_cmdline() {
        let i = classify(&app("cargo", "cargo build"));
        assert_eq!(i.category, Category::Build);
        assert!(i.can_interrupt);
    }

    #[test]
    fn suspend_confidence_scores_by_intent_and_state() {
        let firefox = classify_command("firefox");
        let mut a = app("firefox", "firefox");
        a.cpu_pct = 0.5;
        // calm browser, freshly used
        assert_eq!(suspend_confidence(&a, &firefox, 0), 80);
        // long idle adds confidence
        assert_eq!(suspend_confidence(&a, &firefox, 700), 95);
        // busy cpu drops it
        a.cpu_pct = 50.0;
        assert_eq!(suspend_confidence(&a, &firefox, 0), 30);
        // audio playback is a strong veto
        a.cpu_pct = 0.5;
        a.has_audio = true;
        assert_eq!(suspend_confidence(&a, &firefox, 0), 40);
        // an attached terminal lowers it
        a.has_audio = false;
        a.tty = true;
        assert_eq!(suspend_confidence(&a, &firefox, 0), 55);

        // unknown app, calm and idle
        let unknown = classify_cmd("frobnicate", "frobnicate", false);
        let mut a = app("frobnicate", "frobnicate");
        a.cpu_pct = 0.5;
        assert_eq!(suspend_confidence(&a, &unknown, 700), 85);
    }

    #[test]
    fn suspend_confidence_is_zero_for_protected_intents() {
        // never_suspend: LLM runtimes, sessions, services
        let ollama = classify_command("ollama serve");
        assert_eq!(
            suspend_confidence(&app("ollama", "ollama serve"), &ollama, 3600),
            0
        );
        let ssh = classify_command("ssh user@host");
        assert_eq!(
            suspend_confidence(&app("ssh", "ssh user@host"), &ssh, 3600),
            0
        );
        // can_suspend = false: system plumbing
        let systemd = classify_cmd("systemd", "systemd", false);
        assert_eq!(
            suspend_confidence(&app("systemd", "systemd"), &systemd, 3600),
            0
        );
    }
}
