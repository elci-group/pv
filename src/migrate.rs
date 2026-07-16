//! Device migration: move restartable work to a better machine over ssh.
//!
//! ~/.config/pv/hosts.toml
//! ```toml
//! [hosts.desktop]
//! addr = "sal@192.168.1.20"
//! note = "16 cores, 64 GB, RTX 5090"
//! ```
//!
//! Security notes:
//!   - `host.addr` is validated to reject shell metacharacters and whitespace.
//!   - Remote commands are built as a single argv entry so the local shell
//!     never interprets them; the remote shell receives one quoted string.
//!   - `rsync` and `ssh` are invoked with argument lists, not string concat.

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
    // Drop hosts with unsafe addresses at load time so no later code can use
    // them by accident.
    v.retain(|(_, h)| validate_addr(&h.addr).is_ok());
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

/// Reject addresses that would be unsafe to pass to ssh/rsync argv or that
/// could be interpreted by a shell. We allow the characters that appear in
/// typical `user@host` or bare `host` forms plus `.`, `-`, `_`, digits, `@`,
/// and `:` (for IPv6 literals; callers using IPv6 should use bracket form).
pub fn validate_addr(addr: &str) -> Result<(), String> {
    if addr.is_empty() {
        return Err("host address is empty".into());
    }
    if addr.len() > 253 {
        return Err("host address is too long".into());
    }
    for c in addr.chars() {
        if c.is_alphanumeric()
            || "._-@:[]".contains(c)
        {
            continue;
        }
        return Err(format!(
            "host address '{addr}' contains unsafe character '{c}'"
        ));
    }
    // Defensive: no whitespace or control characters should survive the loop,
    // but reject them explicitly for clarity.
    if addr.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("host address '{addr}' contains whitespace/control characters"));
    }
    Ok(())
}

fn ssh_base(addr: &str) -> Result<Command, String> {
    validate_addr(addr)?;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=3",
        "-o",
        "StrictHostKeyChecking=accept-new",
        addr,
    ]);
    Ok(cmd)
}

/// Quick online check: ssh BatchMode, 3s timeout, true.
pub fn online(addr: &str) -> bool {
    match ssh_base(addr) {
        Ok(mut cmd) => cmd
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        Err(e) => {
            log::warn!("invalid host address '{addr}': {e}");
            false
        }
    }
}

/// Remote capacity probe: cores + mem GB in one ssh round trip.
pub fn probe(addr: &str) -> Option<String> {
    let mut cmd = ssh_base(addr).ok()?;
    let out = cmd
        .arg("echo $(nproc) cores, $(awk '/MemTotal/{printf \"%.0f\", $2/1048576}' /proc/meminfo) GB RAM")
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
    validate_addr(&host.addr)?;
    for tool in ["ssh", "rsync"] {
        if Command::new(tool).arg("--version").output().is_err() {
            return Err(format!("`{tool}` not found — install it to enable migration"));
        }
    }
    if cmd.is_empty() {
        return Err("no command to migrate".into());
    }
    if cwd.contains('\0') {
        return Err("working directory contains a null byte".into());
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

    log::info!("migrating to {}: syncing {cwd} → {remote_dir}", host.addr);
    println!("[pv] syncing {cwd} → {}:{remote_dir}", host.addr);

    let mut rsync = Command::new("rsync");
    rsync.args([
        "-az",
        "--info=progress2",
        "--timeout=120",
        "--exclude",
        "target/",
        "--exclude",
        "node_modules/",
        "--exclude",
        ".git/",
    ]);
    rsync.arg(format!("{cwd}/"));
    rsync.arg(format!("{}:{remote_dir}", host.addr));

    let st = rsync.status().map_err(|e| format!("rsync: {e}"))?;
    if !st.success() {
        return Err(format!("rsync failed ({st})"));
    }

    log::info!("migrating to {}: resuming remotely: {}", host.addr, cmd.join(" "));
    println!("[pv] resuming remotely: {}", cmd.join(" "));

    let remote_cd = shell_quote_keep_tilde(&remote_dir);
    let remote_cmd = shell_join(cmd);
    let script = format!("cd {remote_cd} && {remote_cmd}");

    let mut ssh = ssh_base(&host.addr)?;
    let st = ssh
        .arg(&script)
        .status()
        .map_err(|e| format!("ssh: {e}"))?;
    if st.success() {
        log::info!("migration to {} finished OK", host.addr);
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        "job".into()
    } else {
        out
    }
}

/// Quote a string for remote shell inclusion, but keep a leading `~/`
/// unquoted so the shell still expands it to the remote home directory.
fn shell_quote_keep_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        format!("~/'{}'", rest.replace('\'', "'\\''"))
    } else if s.starts_with('~') {
        // lone ~ or ~user: keep unquoted; this is safe because it cannot
        // contain shell metachars after our sanitization.
        s.to_string()
    } else {
        shell_quote(s)
    }
}

/// Quote a string for remote shell inclusion with standard single-quote
/// escaping. Anything outside [A-Za-z0-9._/=+:,~-] is quoted.
fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_alphanumeric() || "-._/=+:,~".contains(c))
            {
                a.clone()
            } else {
                shell_quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
            shell_quote_keep_tilde("~/.pv/migrated/pv-x---cmd--"),
            "~/'.pv/migrated/pv-x---cmd--'"
        );
        // anything unsafe is single-quoted, so it cannot inject commands
        assert_eq!(
            shell_join(&["~/.pv/migrated/pv-x'; rm -rf / #".into()]),
            "'~/.pv/migrated/pv-x'\\''; rm -rf / #'"
        );
    }

    #[test]
    fn validate_addr_allows_typical_forms() {
        validate_addr("user@192.168.1.20").unwrap();
        validate_addr("my-host.example.com").unwrap();
        validate_addr("root@[2001:db8::1]").unwrap();
    }

    #[test]
    fn validate_addr_rejects_unsafe_chars() {
        assert!(validate_addr("user@host; rm -rf /").is_err());
        assert!(validate_addr("host & cmd").is_err());
        assert!(validate_addr("host`cmd`").is_err());
        assert!(validate_addr("$(cmd)").is_err());
        assert!(validate_addr("").is_err());
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }
}
