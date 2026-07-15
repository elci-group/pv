//! Continuity: `pv run` detaches work from its terminal. SSH can drop,
//! the session survives, and `pv attach` picks it back up.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
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

fn data_home() -> PathBuf {
    crate::procfs::xdg("XDG_DATA_HOME", ".local/share")
}

fn sessions_dir_in(base: &Path) -> PathBuf {
    base.join("pv/sessions")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn list() -> Vec<Session> {
    list_in(&data_home())
}

fn list_in(base: &Path) -> Vec<Session> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(sessions_dir_in(base)) {
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
    find_in(&data_home(), id_or_name)
}

fn find_in(base: &Path, id_or_name: &str) -> Option<Session> {
    let all = list_in(base);
    all.iter()
        .find(|s| s.id == id_or_name)
        .cloned()
        .or_else(|| {
            all.into_iter().find(|s| {
                s.id.starts_with(id_or_name)
                    || s.cmd
                        .join(" ")
                        .to_lowercase()
                        .contains(&id_or_name.to_lowercase())
            })
        })
}

/// Spawn `cmd` detached (setsid, no controlling terminal, output to log).
pub fn run(cmd: &[String], intent_task: &str, intent_category: &str) -> Result<Session, String> {
    run_in(&data_home(), cmd, intent_task, intent_category)
}

fn run_in(
    base: &Path,
    cmd: &[String],
    intent_task: &str,
    intent_category: &str,
) -> Result<Session, String> {
    if cmd.is_empty() {
        return Err("no command given".into());
    }
    let dir = sessions_dir_in(base);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = format!("{:08x}", (now() as u32) ^ (std::process::id()));
    let log = dir.join(format!("{id}.log"));
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
        dir.join(format!("{id}.toml")),
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
    let Ok(mut f) = fs::File::open(&s.log) else {
        return vec![];
    };
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
    gc_in(&data_home())
}

fn gc_in(base: &Path) -> usize {
    let mut removed = 0;
    for s in list_in(base) {
        if s.host == "local" && !is_alive(&s) {
            // keep recent finished sessions for `pv sessions` visibility;
            // only gc those older than 24h
            if now().saturating_sub(s.started) > 86400 {
                let _ = fs::remove_file(sessions_dir_in(base).join(format!("{}.toml", s.id)));
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


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique tempdir per test (pid + sequence + tag); never touches the
    /// real XDG dirs. Caller removes it at the end of the test.
    fn tempdir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pv-session-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("tempdir");
        dir
    }

    fn sess(id: &str, started: u64, pid: u32, host: &str, log: PathBuf) -> Session {
        Session {
            id: id.into(),
            cmd: vec!["sleep".into(), "100".into()],
            pid,
            started,
            intent_task: "task".into(),
            intent_category: "Build".into(),
            cwd: "/tmp".into(),
            log,
            host: host.into(),
        }
    }

    /// Persist a session record exactly where list_in/gc_in look for it.
    fn store(base: &Path, s: &Session) {
        let dir = sessions_dir_in(base);
        fs::create_dir_all(&dir).expect("sessions dir");
        if let Some(parent) = s.log.parent() {
            fs::create_dir_all(parent).expect("log dir");
        }
        fs::write(&s.log, "log line\n").expect("log");
        fs::write(
            dir.join(format!("{}.toml", s.id)),
            toml::to_string(s).expect("serialize"),
        )
        .expect("write record");
    }

    #[test]
    fn run_in_rejects_empty_command() {
        let base = tempdir("empty");
        assert!(run_in(&base, &[], "t", "c").is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn run_in_spawns_detached_and_round_trips() {
        let base = tempdir("run");
        let cmd = vec!["true".to_string()]; // benign: no output, no mutation
        let s = run_in(&base, &cmd, "test task", "Build").expect("run");
        assert_eq!(s.cmd, cmd);
        assert_eq!(s.intent_task, "test task");
        assert_eq!(s.intent_category, "Build");
        assert_eq!(s.host, "local");
        assert!(s.pid != 0);
        assert!(s.log.starts_with(&base));
        assert!(s.log.exists(), "log file created");
        // record persisted and discoverable
        let listed = list_in(&base);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, s.id);
        let found = find_in(&base, &s.id).expect("find by id");
        assert_eq!(found.cmd, cmd);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_in_sorts_by_started_and_skips_unparseable() {
        let base = tempdir("list");
        let dir = sessions_dir_in(&base);
        fs::create_dir_all(&dir).expect("dir");
        for (id, started) in [("c", 300u64), ("a", 100), ("b", 200)] {
            store(&base, &sess(id, started, u32::MAX, "local", dir.join(format!("{id}.log"))));
        }
        fs::write(dir.join("garbage.toml"), "not [a session").expect("garbage");
        fs::write(dir.join("wrongshape.toml"), "foo = 1\n").expect("wrong shape");
        fs::write(dir.join("notes.txt"), "id = \"zzz\"\n").expect("non-toml");
        let ids: Vec<String> = list_in(&base).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn find_in_matches_exact_prefix_and_command() {
        let base = tempdir("find");
        let dir = sessions_dir_in(&base);
        store(&base, &sess("a", 1, u32::MAX, "local", dir.join("a.log")));
        let mut other = sess("abc", 2, u32::MAX, "local", dir.join("abc.log"));
        other.cmd = vec!["cargo".into(), "build".into()];
        store(&base, &other);
        // exact id wins over the prefix-colliding "abc"
        assert_eq!(find_in(&base, "a").map(|s| s.id), Some("a".into()));
        // unique prefix
        assert_eq!(find_in(&base, "ab").map(|s| s.id), Some("abc".into()));
        // case-insensitive command substring
        assert_eq!(find_in(&base, "CARGO").map(|s| s.id), Some("abc".into()));
        assert!(find_in(&base, "nope").is_none());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn tail_returns_last_n_lines() {
        let base = tempdir("tail");
        let log = base.join("t.log");
        fs::write(&log, "l1\nl2\nl3\nl4\nl5\n").expect("log");
        let s = sess("t", 0, u32::MAX, "local", log);
        assert_eq!(tail(&s, 2), vec!["l4".to_string(), "l5".to_string()]);
        assert_eq!(tail(&s, 99).len(), 5);
        assert_eq!(tail(&s, 0).len(), 0);
        // missing log: empty, not a panic
        let gone = sess("gone", 0, u32::MAX, "local", base.join("nope.log"));
        assert!(tail(&gone, 3).is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn is_alive_checks_proc_only_for_local_sessions() {
        let me = std::process::id();
        let local_alive = sess("x", 0, me, "local", PathBuf::new());
        assert!(is_alive(&local_alive));
        // 4294967295 cannot be a live pid (Linux pid_max tops out far lower)
        let local_dead = sess("y", 0, u32::MAX, "local", PathBuf::new());
        assert!(!is_alive(&local_dead));
        // remote liveness is ssh's problem; assumed alive here
        let remote = sess("z", 0, u32::MAX, "otherhost", PathBuf::new());
        assert!(is_alive(&remote));
    }

    #[test]
    fn gc_in_removes_only_old_dead_local_sessions() {
        let base = tempdir("gc");
        let dir = sessions_dir_in(&base);
        let old = now() - 90_000; // > 24h ago
        store(&base, &sess("dead-old", old, u32::MAX, "local", dir.join("dead-old.log")));
        store(&base, &sess("dead-new", now() - 10, u32::MAX, "local", dir.join("dead-new.log")));
        store(&base, &sess("alive-old", old, std::process::id(), "local", dir.join("alive.log")));
        store(&base, &sess("remote-old", old, u32::MAX, "otherhost", dir.join("remote.log")));
        // clock skew: started in the future must saturate to 0 age, not wrap
        store(&base, &sess("future", now() + 90_000, u32::MAX, "local", dir.join("future.log")));

        assert_eq!(gc_in(&base), 1);
        assert!(!dir.join("dead-old.toml").exists(), "record removed");
        assert!(!dir.join("dead-old.log").exists(), "log removed");
        for kept in ["dead-new", "alive-old", "remote-old", "future"] {
            assert!(dir.join(format!("{kept}.toml")).exists(), "{kept} kept");
        }
        let _ = fs::remove_dir_all(&base);
    }
}
