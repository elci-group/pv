//! Device migration: move restartable work to a better machine over ssh.
//!
//! ~/.config/pv/hosts.toml
//! ```toml
//! [hosts.desktop]
//! addr = "sal@192.168.1.20"
//! note = "16 cores, 64 GB, RTX 5090"
//! ```

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Host {
    pub addr: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, serde::Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: std::collections::HashMap<String, Host>,
}

fn hosts_path() -> PathBuf {
    crate::procfs::xdg("XDG_CONFIG_HOME", ".config").join("pv/hosts.toml")
}

pub fn load_hosts() -> Vec<(String, Host)> {
    let mut v: Vec<_> = fs::read_to_string(hosts_path())
        .ok()
        .and_then(|s| toml::from_str::<HostsFile>(&s).ok())
        .map(|h| h.hosts.into_iter().collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn default_hosts() -> &'static str {
    r#"# Pressure Valve remote hosts — used by `pv migrate`
# [hosts.desktop]
# addr = "user@192.168.1.20"
# note = "16 cores, 64 GB, RTX 5090"
"#
}

/// Quick online check: ssh BatchMode, 3s timeout, true.
pub fn online(addr: &str) -> bool {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=3", addr, "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Remote capacity probe: cores + mem GB in one ssh round trip.
pub fn probe(addr: &str) -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes", "-o", "ConnectTimeout=3",
            addr,
            "echo $(nproc) cores, $(awk '/MemTotal/{printf \"%.0f\", $2/1048576}' /proc/meminfo) GB RAM",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Migrate a restartable command to a remote host:
/// rsync the working directory across, run the command there, stream output.
pub fn migrate_command(host: &Host, cmd: &[String], cwd: &str) -> Result<(), String> {
    for tool in ["ssh", "rsync"] {
        if Command::new(tool).arg("--version").output().is_err() {
            return Err(format!("`{tool}` not found — install it to enable migration"));
        }
    }
    let raw_name = cmd
        .first()
        .map(|c| {
            std::path::Path::new(c)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "job".into())
        })
        .unwrap_or_else(|| "job".into());
    let job = format!("pv-{}", sanitize_job(&raw_name));
    let remote_dir = format!("~/.pv/migrated/{job}");

    println!("[pv] syncing {cwd} → {}:{remote_dir}", host.addr);
    let st = Command::new("rsync")
        .args([
            "-az", "--info=progress2",
            "--exclude", "target/", "--exclude", "node_modules/", "--exclude", ".git/",
            &format!("{cwd}/"),
            &format!("{}:{remote_dir}", host.addr),
        ])
        .status()
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("rsync failed".into());
    }

    println!("[pv] resuming remotely: {}", cmd.join(" "));
    let st = Command::new("ssh")
        .args([
            &host.addr.clone(),
            &format!("cd {} && {}", shell_join(std::slice::from_ref(&remote_dir)), shell_join(cmd)),
        ])
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        println!("[pv] remote run finished OK — local copy untouched in {cwd}");
        Ok(())
    } else {
        Err(format!("remote command exited with {st}"))
    }
}

/// Reduce an executable basename to [A-Za-z0-9._-] so it is safe to embed
/// in the remote dir name and the remote shell command.
fn sanitize_job(name: &str) -> String {
    let out: String = name
        .chars()
        .take(40)
        .map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '-' })
        .collect();
    if out.is_empty() { "job".into() } else { out }
}

fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| {
            // `~` is safe unquoted: it can only trigger home-dir expansion
            if a.chars().all(|c| c.is_alphanumeric() || "-._/=+:,~".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_job_strips_shell_metachars() {
        assert_eq!(sanitize_job("x'; cmd #"), "x---cmd--");
        assert_eq!(sanitize_job("cargo"), "cargo");
        assert_eq!(sanitize_job("a/b\0c"), "a-b-c");
        assert_eq!(sanitize_job(""), "job");
        assert_eq!(sanitize_job("ünïcode"), "-n-code");
        assert_eq!(sanitize_job(&"a".repeat(100)), "a".repeat(40));
    }

    #[test]
    fn shell_join_quotes_unsafe_args() {
        assert_eq!(shell_join(&["ls".into(), "-la".into()]), "ls -la");
        // sanitized remote dir keeps `~/` unquoted so the tilde still expands
        assert_eq!(
            shell_join(&["~/.pv/migrated/pv-x---cmd--".into()]),
            "~/.pv/migrated/pv-x---cmd--"
        );
        // anything unsafe is single-quoted, so it cannot inject commands
        assert_eq!(
            shell_join(&["~/.pv/migrated/pv-x'; rm -rf / #".into()]),
            "'~/.pv/migrated/pv-x'\\''; rm -rf / #'"
        );
    }
}
