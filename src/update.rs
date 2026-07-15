//! `pv update` — self-update: latest GitHub release binary, or clone+build
//! from source. Release binaries are verified against the release's
//! SHA256SUMS before install (fail closed on mismatch or missing sums).
//! Installs to ~/.local/bin (user) or /usr/local/bin (--system).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::display::Theme;
use crate::groq::extract_field;

pub const DEFAULT_REPO: &str = "elci-group/pv";
// Release assets: the binary `pv-linux-x86_64` plus a `SHA256SUMS` file.
// The updater selects both by asset name and verifies the downloaded
// binary against its sums line before installing.
const BINARY_ASSET: &str = "pv-linux-x86_64";
const SUMS_ASSET: &str = "SHA256SUMS";

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

/// Latest GitHub release: (tag, binary url, SHA256SUMS url). None when the
/// repo has no releases yet. Errors when the release lacks the expected
/// `pv-linux-x86_64` asset; a missing SHA256SUMS asset comes back as None
/// and blocks the install (checksum verification is mandatory).
pub fn latest_release(repo: &str) -> Result<Option<(String, String, Option<String>)>, String> {
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
    let asset = asset_url(&body, BINARY_ASSET)
        .ok_or_else(|| format!("release {tag} has no {BINARY_ASSET} asset"))?;
    let sums = asset_url(&body, SUMS_ASSET);
    Ok(Some((tag, asset, sums)))
}

/// Download URL of the release asset named `name` in a GitHub release JSON
/// body. Scans each asset's `"name"` and takes the `"browser_download_url"`
/// that follows it, so multi-asset releases pick the right file instead of
/// the first asset listed.
fn asset_url(json: &str, name: &str) -> Option<String> {
    let assets = &json[json.find("\"assets\"")?..];
    let mut rest = assets;
    while let Some(pos) = rest.find("\"name\"") {
        let after = &rest[pos + 6..];
        let colon = after.find(':')?;
        let value = after[colon + 1..].trim_start();
        if !value.starts_with('"') {
            rest = after;
            continue;
        }
        let end = value[1..].find('"')?;
        if &value[1..1 + end] == name {
            return extract_field(&value[1 + end..], "browser_download_url");
        }
        rest = &value[1 + end..];
    }
    None
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

/// Copy `binary` into the fresh temp file `out`, flush it to disk, then
/// atomically rename it over `dest`.
fn stage(out: &mut fs::File, binary: &Path, tmp: &Path, dest: &Path) -> Result<(), String> {
    let mut src =
        fs::File::open(binary).map_err(|e| format!("read {}: {e}", binary.display()))?;
    std::io::copy(&mut src, out).map_err(|e| format!("copy: {e}"))?;
    out.sync_all().map_err(|e| format!("fsync: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(tmp, fs::Permissions::from_mode(0o755));
    }
    // atomic swap; replacing a running binary is fine on Linux
    fs::rename(tmp, dest).map_err(|e| format!("install to {}: {e}", dest.display()))
}

fn install(binary: &Path, system: bool) -> Result<PathBuf, String> {
    let dir = install_dir(system);
    let dest = dir.join("pv");
    if !dir.exists() && !system {
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    if dir_writable(&dir) {
        // pid-unique temp name + create_new: concurrent updaters never share
        // a file, so nobody can rename a half-written binary
        let tmp = dir.join(format!(".pv-new-{}", std::process::id()));
        let mut out = match fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "{} exists (stale temp from an interrupted update?) — remove it and retry",
                    tmp.display()
                ));
            }
            Err(e) => return Err(format!("create {}: {e}", tmp.display())),
        };
        let result = stage(&mut out, binary, &tmp, &dest);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result.map(|()| dest);
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
        .args(["-fsSL", "-H", "User-Agent: pv-updater", "-o"])
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

/// Lowercased sha256 hash for `name` in a SHA256SUMS body
/// (`<64-hex>  <name>` per line, `*name` in binary mode). None when the
/// line is absent or the hash field is not 64 hex chars.
fn sums_entry(sums: &str, name: &str) -> Option<String> {
    for line in sums.lines() {
        let mut it = line.split_whitespace();
        let (hash, file) = match (it.next(), it.next()) {
            (Some(h), Some(f)) => (h, f),
            _ => continue,
        };
        if file.trim_start_matches('*') == name
            && hash.len() == 64
            && hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// sha256 of `path`, computed with the coreutils `sha256sum` tool.
fn sha256_of(path: &Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("`sha256sum` (coreutils) is required to verify downloads: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sha256sum {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "sha256sum produced no output".into())
}

/// Fail closed unless `binary`'s sha256 matches its line in `sums_body`.
fn verify(binary: &Path, sums_body: &str) -> Result<(), String> {
    let want = sums_entry(sums_body, BINARY_ASSET).ok_or_else(|| {
        format!("{SUMS_ASSET} has no line for {BINARY_ASSET} — refusing to install")
    })?;
    let got = sha256_of(binary)?;
    if got != want {
        return Err(format!(
            "checksum mismatch for {BINARY_ASSET} (expected {want}, got {got}) — refusing to install"
        ));
    }
    Ok(())
}

pub fn run(t: &Theme, o: &Options) -> i32 {
    let cur = current_version();
    println!("{} {cur}", t.dim("current:"));

    if !o.source {
        match latest_release(&o.repo) {
            Ok(Some((tag, url, sums_url))) => {
                println!("{} {tag} (release asset)", t.dim("latest:"));
                if !o.force && !is_newer(&tag, cur) {
                    println!("{}", t.green("✓ already up to date"));
                    return 0;
                }
                if o.check {
                    println!("update available: {cur} → {tag}");
                    return 0;
                }
                let sums_url = match sums_url {
                    Some(u) => u,
                    None => {
                        eprintln!(
                            "[pv] release {tag} has no {SUMS_ASSET} asset — refusing to install an unverified binary"
                        );
                        return 1;
                    }
                };
                let tmp = std::env::temp_dir().join(format!("pv-update-{}", std::process::id()));
                let _ = fs::create_dir_all(&tmp);
                let bin = tmp.join("pv");
                let sums = tmp.join(SUMS_ASSET);
                print!("{}", t.dim("downloading… "));
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let outcome = download(&url, &bin)
                    .and_then(|()| download(&sums_url, &sums))
                    .and_then(|()| {
                        fs::read_to_string(&sums)
                            .map_err(|e| format!("read {}: {e}", sums.display()))
                    })
                    .and_then(|body| verify(&bin, &body))
                    .and_then(|()| install(&bin, o.system));
                match outcome {
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

    #[test]
    fn picks_named_asset() {
        let json = r#"{"tag_name":"v0.5.0","name":"pv 0.5.0","assets":[
            {"name":"pv-linux-aarch64","browser_download_url":"https://x/arm"},
            {"name":"pv-linux-x86_64","browser_download_url":"https://x/x64"},
            {"name":"SHA256SUMS","browser_download_url":"https://x/sums"}
        ]}"#;
        assert_eq!(asset_url(json, "pv-linux-x86_64").as_deref(), Some("https://x/x64"));
        assert_eq!(asset_url(json, "SHA256SUMS").as_deref(), Some("https://x/sums"));
        assert_eq!(asset_url(json, "pv-windows.exe"), None);
    }

    #[test]
    fn asset_url_tolerates_whitespace() {
        let json = r#"{ "assets": [ { "name": "pv-linux-x86_64",
            "browser_download_url": "https://x/pv" } ] }"#;
        assert_eq!(asset_url(json, "pv-linux-x86_64").as_deref(), Some("https://x/pv"));
        // no assets array at all → no url
        assert_eq!(asset_url("{\"tag_name\":\"v1\"}", "pv-linux-x86_64"), None);
    }

    #[test]
    fn parses_sums_lines() {
        let sums = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  pv-linux-aarch64\n\
                    e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  pv-linux-x86_64\n";
        assert_eq!(
            sums_entry(sums, "pv-linux-x86_64").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(sums_entry(sums, "pv-windows.exe"), None);
        assert_eq!(sums_entry("", "pv-linux-x86_64"), None);
    }

    #[test]
    fn sums_line_accepts_binary_marker_and_uppercase() {
        let sums = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855 *pv-linux-x86_64\n";
        assert_eq!(
            sums_entry(sums, "pv-linux-x86_64").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    #[test]
    fn rejects_malformed_sums_lines() {
        // short / non-hex hash fields are not checksum lines
        assert_eq!(sums_entry("deadbeef  pv-linux-x86_64\n", "pv-linux-x86_64"), None);
        let bad = format!("{}  pv-linux-x86_64\n", "z".repeat(64));
        assert_eq!(sums_entry(&bad, "pv-linux-x86_64"), None);
        // filename is only a prefix of the wanted name
        assert_eq!(sums_entry("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  pv-linux-x86_64.sig\n", "pv-linux-x86_64"), None);
    }
}
