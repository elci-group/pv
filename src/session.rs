//! Continuity: `pv run` detaches work from its terminal. SSH can drop,
//! the session survives, and `pv attach` picks it back up.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub id: String,
    pub cmd: Vec<String>,
    pub pid: u32,
    pub started: u64,
    pub intent_task: String,
    pub intent_category: String,
    pub cwd: String,
    pub log: PathBuf,
    pub host: String, // "local" or ssh target
}

fn sessions_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/share")
        });
    base.join("pv/sessions")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn list() -> Vec<Session> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "toml").unwrap_or(false) {
                if let Ok(s) = fs::read_to_string(&p) {
                    if let Ok(sess) = toml::from_str::<Session>(&s) {
                        out.push(sess);
                    }
                }
            }
        }
    }
    out.sort_by_key(|s| s.started);
    out
}

pub fn is_alive(s: &Session) -> bool {
    if s.host != "local" {
        return true; // remote liveness is checked over ssh, not /proc
    }
    PathBuf::from(format!("/proc/{}", s.pid)).exists()
}

pub fn find(id_or_name: &str) -> Option<Session> {
    let all = list();
    all.iter()
        .find(|s| s.id == id_or_name)
        .cloned()
        .or_else(|| {
            all.into_iter().find(|s| {
                s.id.starts_with(id_or_name)
                    || s.cmd.join(" ").to_lowercase().contains(&id_or_name.to_lowercase())
            })
        })
}

/// Spawn `cmd` detached (setsid, no controlling terminal, output to log).
pub fn run(cmd: &[String], intent_task: &str, intent_category: &str) -> Result<Session, String> {
    if cmd.is_empty() {
        return Err("no command given".into());
    }
    fs::create_dir_all(sessions_dir()).map_err(|e| e.to_string())?;
    let id = format!("{:08x}", (now() as u32) ^ (std::process::id()));
    let log = sessions_dir().join(format!("{id}.log"));
    let logf = fs::File::create(&log).map_err(|e| e.to_string())?;
    let errf = logf.try_clone().map_err(|e| e.to_string())?;

    // preexec: setsid() so the session survives terminal/SSH death
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(errf));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc_setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .map_err(|e| format!("spawn '{}': {e}", cmd[0]))?;

    let sess = Session {
        id: id.clone(),
        cmd: cmd.to_vec(),
        pid: child.id(),
        started: now(),
        intent_task: intent_task.to_string(),
        intent_category: intent_category.to_string(),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        log,
        host: "local".into(),
    };
    fs::write(
        sessions_dir().join(format!("{id}.toml")),
        toml::to_string_pretty(&sess).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(sess)
}

extern "C" {
    fn setsid() -> i32;
}
fn libc_setsid() -> i32 {
    unsafe { setsid() }
}

/// Read the last `n` lines of a session log.
pub fn tail(s: &Session, n: usize) -> Vec<String> {
    let Ok(mut f) = fs::File::open(&s.log) else { return vec![] };
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    let lines: Vec<&str> = buf.lines().collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(n))
        .map(|s| s.to_string())
        .collect()
}

/// Remove finished session records (keep logs' metadata tidy).
pub fn gc() -> usize {
    let mut removed = 0;
    for s in list() {
        if s.host == "local" && !is_alive(&s) {
            // keep recent finished sessions for `pv sessions` visibility;
            // only gc those older than 24h
            if now().saturating_sub(s.started) > 86400 {
                let _ = fs::remove_file(sessions_dir().join(format!("{}.toml", s.id)));
                let _ = fs::remove_file(&s.log);
                removed += 1;
            }
        }
    }
    removed
}

pub fn follow(s: &Session) -> std::io::Result<()> {
    // tail -f equivalent
    let mut f = fs::File::open(&s.log)?;
    let mut pos = f.metadata().map(|m| m.len()).unwrap_or(0);
    // show existing content first
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    print!("{}", String::from_utf8_lossy(&buf));
    loop {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len > pos {
            use std::io::{Seek, SeekFrom};
            let _ = f.seek(SeekFrom::Start(pos));
            let mut chunk = Vec::new();
            let _ = f.read_to_end(&mut chunk);
            print!("{}", String::from_utf8_lossy(&chunk));
            use std::io::Write;
            let _ = std::io::stdout().flush();
            pos = len;
        }
        if s.host == "local" && !is_alive(s) {
            println!("\n[pv] session {} exited", s.id);
            return Ok(());
        }
    }
}
