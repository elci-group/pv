//! Shared curl-based HTTP transport — the one place curl is spawned, so
//! token handling, timeouts, and error shaping stay consistent. Keeps
//! HTTP/TLS crates out of the dependency tree at the cost of requiring the
//! `curl` binary at runtime (see `pv doctor`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn have_curl() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// GET `url`, returning the body. `headers` are `Name: value` pairs passed
/// via repeated `-H`. Fails on spawn error, non-success curl exit, or
/// non-UTF-8 body.
pub fn get_string(url: &str, headers: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "30"]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    let out = cmd.arg(url).output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "request failed ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// GET `url` into `dest`. Same semantics as [`get_string`].
pub fn download(url: &str, dest: &Path, headers: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "300"]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    let st = cmd
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("download failed ({st})"))
    }
}

/// Curl config file carrying an Authorization header; the file is removed
/// when this guard drops. Keeps the bearer token out of curl's argv, which
/// is world-readable via /proc/<pid>/cmdline for the life of the process.
pub struct BearerConfig(PathBuf);

impl BearerConfig {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for BearerConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `header = "Authorization: Bearer <key>"` to a fresh 0600 file under
/// the pv state dir, for curl's `-K`. Refuses keys containing characters that
/// could break out of the quoted config value.
pub fn write_bearer_config(key: &str) -> Result<BearerConfig, String> {
    if key.contains('"') || key.contains('\n') || key.contains('\r') {
        return Err("API key contains characters unsafe for a curl config file".into());
    }
    use std::os::unix::fs::OpenOptionsExt;
    let dir = crate::procfs::xdg("XDG_DATA_HOME", ".local/share").join("pv");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("curl-{}-{seq}.conf", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    writeln!(f, "header = \"Authorization: Bearer {key}\"")
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(BearerConfig(path))
}

/// Spawn a streaming POST (`curl -sN`, SSE-style) with bearer auth via `-K`
/// config; `payload` is written to curl's stdin. The caller reads the
/// response body line-by-line from the returned child's stdout and is
/// responsible for wait/kill.
pub fn spawn_streaming_post(
    url: &str,
    auth: &BearerConfig,
    payload: &str,
) -> Result<std::process::Child, String> {
    let mut child = Command::new("curl")
        .args(["-sN", "-X", "POST", url, "-K"])
        .arg(auth.path())
        .args([
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "--max-time",
            "30",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("curl spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_config_is_0600_and_carries_header() {
        let cfg = write_bearer_config("gsk_test123").expect("config");
        let path = cfg.path().to_path_buf();
        assert!(path.starts_with(crate::procfs::xdg("XDG_DATA_HOME", ".local/share").join("pv")));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body, "header = \"Authorization: Bearer gsk_test123\"\n");
        drop(cfg);
        assert!(!path.exists(), "config file removed on drop");
    }

    #[test]
    fn bearer_config_rejects_unsafe_keys() {
        assert!(write_bearer_config("bad\"key").is_err());
        assert!(write_bearer_config("bad\nkey").is_err());
        assert!(write_bearer_config("bad\rkey").is_err());
    }

    #[test]
    fn bearer_config_paths_are_unique() {
        let a = write_bearer_config("k1").expect("a");
        let b = write_bearer_config("k2").expect("b");
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn get_string_reports_failure() {
        // loopback port 9 (discard): connection refused, fails fast
        let r = get_string("http://127.0.0.1:9/", &[]);
        assert!(r.is_err());
    }
}
