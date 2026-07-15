//! `pv update` — self-update: latest GitHub release binary, or clone+build
//! from source. Installs to ~/.local/bin (user) or /usr/local/bin (--system).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::display::Theme;
use crate::groq::extract_field;

pub const DEFAULT_REPO: &str = "elci-group/pv";
pub const ASSET_NAME: &str = "pv-linux-x86_64";

#[derive(Debug, Clone)]
pub struct Options {
    pub source: bool,
    pub system: bool,
    pub force: bool,
    pub check: bool,
    pub repo: String,
}

// ---------------------------------------------------------------- helpers

fn curl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: pv-updater"])
        .args(args)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "request failed ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn parse_ver(s: &str) -> (u32, u32, u32) {
    let mut it = s.trim().trim_start_matches('v').split('.');
    let n = |p: Option<&str>| p.and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    (n(it.next()), n(it.next()), n(it.next()))
}

/// true when `latest` is strictly newer than `current`
pub fn is_newer(latest: &str, current: &str) -> bool {
    parse_ver(latest) > parse_ver(current)
}

// ---------------------------------------------------------------- remote facts

/// Latest GitHub release: (tag, asset download url). None when the repo has
/// no releases yet.
pub fn latest_release(repo: &str) -> Result<Option<(String, String)>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body = match curl(&[&url]) {
        Ok(b) => b,
        Err(e) => {
            if e.contains("404") || e.contains("Not Found") {
                return Ok(None);
            }
            return Err(e);
        }
    };
    if body.contains("\"Not Found\"") {
        return Ok(None);
    }
    let tag = extract_field(&body, "tag_name").ok_or("release JSON had no tag_name")?;
    let asset = extract_field(&body, "browser_download_url")
        .ok_or("release has no downloadable assets")?;
    Ok(Some((tag, asset)))
}

/// Version string in the remote main branch's Cargo.toml (cheap pre-check
/// for source builds).
pub fn remote_source_version(repo: &str) -> Result<String, String> {
    let body = curl(&[&format!(
        "https://raw.githubusercontent.com/{repo}/main/Cargo.toml"
    )])?;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("version") {
            if let Some(q) = v.trim().strip_prefix('=') {
                return Ok(q.trim().trim_matches('"').to_string());
            }
        }
    }
    Err("no version in remote Cargo.toml".into())
}

// ---------------------------------------------------------------- install

fn install_dir(system: bool) -> PathBuf {
    if system {
        return PathBuf::from("/usr/local/bin");
    }
    // stay where we are if already installed system-wide
    if let Ok(exe) = std::env::current_exe() {
        if exe.starts_with("/usr/local/bin") {
            return PathBuf::from("/usr/local/bin");
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/bin")
}

fn dir_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(".pv-write-test");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn install(binary: &Path, system: bool) -> Result<PathBuf, String> {
    let dir = install_dir(system);
    let dest = dir.join("pv");
    if !dir.exists() && !system {
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    if dir_writable(&dir) {
        let tmp = dir.join(".pv-new");
        fs::copy(binary, &tmp).map_err(|e| format!("copy: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
        }
        // atomic swap; replacing a running binary is fine on Linux
        fs::rename(&tmp, &dest).map_err(|e| format!("install to {}: {e}", dest.display()))?;
        return Ok(dest);
    }
    // no write access → sudo install (covers --system and odd perms)
    let st = Command::new("sudo")
        .args(["install", "-m", "755"])
        .arg(binary)
        .arg(&dest)
        .status()
        .map_err(|e| format!("sudo: {e}"))?;
    if st.success() {
        Ok(dest)
    } else {
        Err(format!("sudo install failed ({st})"))
    }
}

// ---------------------------------------------------------------- flows

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let st = Command::new("curl")
        .args(["-fSL", "-H", "User-Agent: pv-updater", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("download failed ({st})"))
    }
}

pub fn run(t: &Theme, o: &Options) -> i32 {
    let cur = current_version();
    println!("{} {cur}", t.dim("current:"));

    if !o.source {
        match latest_release(&o.repo) {
            Ok(Some((tag, url))) => {
                println!("{} {tag} (release asset)", t.dim("latest:"));
                if !o.force && !is_newer(&tag, cur) {
                    println!("{}", t.green("✓ already up to date"));
                    return 0;
                }
                if o.check {
                    println!("update available: {cur} → {tag}");
                    return 0;
                }
                let tmp = std::env::temp_dir().join(format!("pv-update-{}", std::process::id()));
                let _ = fs::create_dir_all(&tmp);
                let bin = tmp.join("pv");
                print!("{}", t.dim("downloading… "));
                let _ = std::io::Write::flush(&mut std::io::stdout());
                match download(&url, &bin).and_then(|()| install(&bin, o.system)) {
                    Ok(dest) => {
                        let _ = fs::remove_dir_all(&tmp);
                        println!("\n{} {cur} → {tag} → {}", t.green("✓ updated"), dest.display());
                        path_hint(t, &dest);
                        return 0;
                    }
                    Err(e) => {
                        let _ = fs::remove_dir_all(&tmp);
                        eprintln!("\n[pv] {e}");
                        return 1;
                    }
                }
            }
            Ok(None) => {
                println!("{}", t.dim("no GitHub releases yet — building from source"));
            }
            Err(e) => {
                eprintln!("[pv] release check failed: {e} — trying source build");
            }
        }
    }

    // ---- source build ----
    match remote_source_version(&o.repo) {
        Ok(v) => {
            println!("{} {v} (git main)", t.dim("latest:"));
            if !o.force && !is_newer(&v, cur) {
                println!("{}", t.green("✓ already up to date"));
                return 0;
            }
            if o.check {
                println!("update available: {cur} → {v}");
                return 0;
            }
        }
        Err(e) => {
            eprintln!("[pv] cannot read remote version: {e}");
            return 1;
        }
    }
    for tool in ["git", "cargo"] {
        if Command::new(tool).arg("--version").output().is_err() {
            eprintln!("[pv] `{tool}` is required for --source builds");
            return 1;
        }
    }
    let tmp = std::env::temp_dir().join(format!("pv-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    println!("{} cloning {}…", t.dim("»"), o.repo);
    let st = Command::new("git")
        .args(["clone", "--depth", "1", &format!("https://github.com/{}.git", o.repo)])
        .arg(&tmp)
        .status();
    if !matches!(st, Ok(s) if s.success()) {
        eprintln!("[pv] clone failed");
        let _ = fs::remove_dir_all(&tmp);
        return 1;
    }
    println!("{} building release…", t.dim("»"));
    let st = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&tmp)
        .status();
    let bin = tmp.join("target/release/pv");
    match st {
        Ok(s) if s.success() && bin.exists() => match install(&bin, o.system) {
            Ok(dest) => {
                let _ = fs::remove_dir_all(&tmp);
                println!("{} built and installed → {}", t.green("✓"), dest.display());
                path_hint(t, &dest);
                0
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                eprintln!("[pv] {e}");
                1
            }
        },
        _ => {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!("[pv] build failed");
            1
        }
    }
}

fn path_hint(t: &Theme, dest: &Path) {
    let dir = dest.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let in_path = std::env::var("PATH")
        .map(|p| p.split(':').any(|d| d == dir))
        .unwrap_or(false);
    if !in_path {
        println!(
            "{}",
            t.yellow(&format!("note: {dir} is not on your PATH — add it or run `{}/pv`", dest.display()))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_ver("v0.4.5"), (0, 4, 5));
        assert_eq!(parse_ver("1.2.3"), (1, 2, 3));
        assert_eq!(parse_ver("0.10"), (0, 10, 0));
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.5.0", "0.4.9"));
        assert!(is_newer("0.4.10", "0.4.9"));
        assert!(!is_newer("0.4.5", "0.4.5"));
        assert!(!is_newer("v0.4.4", "0.4.5"));
    }
}
