//! Fast, machine-readable resource admission for latency-sensitive callers.

use std::path::Path;

const RAM_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
const STORAGE_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
const MEMORY_PSI_DENY: f64 = 25.0;
const IO_PSI_PERSIST_DENY: f64 = 50.0;

pub struct Request<'a> {
    pub ram_bytes: u64,
    pub storage_bytes: u64,
    pub path: &'a Path,
    pub json: bool,
}

pub fn run(req: Request<'_>) -> i32 {
    let mem = crate::procfs::meminfo();
    let mem_psi = crate::procfs::psi("memory").unwrap_or_default();
    let io_psi = crate::procfs::psi("io").unwrap_or_default();
    let available_ram = mem.available_kb.saturating_mul(1024);
    let ram_capacity = available_ram.saturating_sub(RAM_RESERVE_BYTES);
    let ram_ok = req.ram_bytes <= ram_capacity && mem_psi.full_avg10 < MEMORY_PSI_DENY;

    let filesystems = crate::storage::filesystems();
    let probe = req
        .path
        .canonicalize()
        .unwrap_or_else(|_| req.path.to_path_buf());
    let fs = filesystems
        .iter()
        .filter(|f| probe.starts_with(&f.mount))
        .max_by_key(|f| f.mount.len());
    let (available_storage, used_pct, mount, measured) = match fs {
        Some(f) => (
            f.avail_kb.saturating_mul(1024),
            f.used_pct,
            f.mount.as_str(),
            true,
        ),
        None => (0, 100, "", false),
    };
    let storage_capacity = available_storage.saturating_sub(STORAGE_RESERVE_BYTES);
    let storage_ok = measured
        && req.storage_bytes <= storage_capacity
        && used_pct < crate::storage::PRESSURE_PCT
        && io_psi.full_avg10 < IO_PSI_PERSIST_DENY;

    let code = match (ram_ok, storage_ok) {
        (true, true) => 0,
        (false, true) => 2,
        (true, false) => 3,
        (false, false) => 4,
    };
    if req.json {
        println!(
            "{{\"ram\":{{\"admitted\":{ram_ok},\"required_bytes\":{},\"available_bytes\":{available_ram},\"reserve_bytes\":{RAM_RESERVE_BYTES},\"psi_full_avg10\":{:.2}}},\"storage\":{{\"admitted\":{storage_ok},\"required_bytes\":{},\"available_bytes\":{available_storage},\"reserve_bytes\":{STORAGE_RESERVE_BYTES},\"used_pct\":{used_pct},\"mount\":\"{}\",\"io_psi_full_avg10\":{:.2}}},\"exit_code\":{code}}}",
            req.ram_bytes,
            mem_psi.full_avg10,
            req.storage_bytes,
            escape(mount),
            io_psi.full_avg10,
        );
    } else {
        println!(
            "ram={} storage={} mount={} exit={code}",
            if ram_ok { "admit" } else { "deny" },
            if storage_ok { "admit" } else { "deny" },
            mount
        );
    }
    code
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn json_escape_is_stable() {
        assert_eq!(escape("/a/\"b"), "/a/\\\"b");
    }
}
