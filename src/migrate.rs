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
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".config")
        });
    base.join("pv/hosts.toml")
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
    let job = format!(
        "pv-{}",
        cmd.first()
            .map(|c| {
                std::path::Path::new(c)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "job".into())
            })
            .unwrap_or_else(|| "job".into())
    );
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
            &format!("cd {remote_dir} && {}", shell_join(cmd)),
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

fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| {
            if a.chars().all(|c| c.is_alphanumeric() || "-._/=+:,".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
