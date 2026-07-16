//! `pv doctor` — preflight: the external tools and config pv relies on.
//!
//! pv keeps its crate dependencies tiny (clap/serde/toml) by shelling out
//! to standard Unix tools; doctor verifies the environment provides them
//! and reports which optional features go dark when a tool is absent.

use crate::display::Theme;

struct Probe {
    tool: &'static str,
    required: bool,
    used_for: &'static str,
}

const PROBES: &[Probe] = &[
    Probe {
        tool: "curl",
        required: true,
        used_for: "Groq inference, pv update",
    },
    Probe {
        tool: "sha256sum",
        required: true,
        used_for: "update checksum verification",
    },
    Probe {
        tool: "ssh",
        required: false,
        used_for: "pv migrate, pv run --remote",
    },
    Probe {
        tool: "rsync",
        required: false,
        used_for: "pv migrate file copy",
    },
    Probe {
        tool: "git",
        required: false,
        used_for: "pv update --source",
    },
    Probe {
        tool: "cargo",
        required: false,
        used_for: "pv update --source",
    },
    Probe {
        tool: "notify-send",
        required: false,
        used_for: "desktop valve notifications",
    },
    Probe {
        tool: "getconf",
        required: false,
        used_for: "precise CPU tick rate",
    },
    Probe {
        tool: "renice",
        required: false,
        used_for: "policy throttle actions",
    },
    Probe {
        tool: "ionice",
        required: false,
        used_for: "policy throttle actions",
    },
    Probe {
        tool: "stty",
        required: false,
        used_for: "pv live terminal size",
    },
];

/// True when `tool` resolves to an executable regular file on $PATH.
pub fn on_path(tool: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&paths).any(|dir| {
        std::fs::metadata(dir.join(tool))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn config_exists(rel: &str) -> bool {
    crate::procfs::xdg("XDG_CONFIG_HOME", ".config")
        .join("pv")
        .join(rel)
        .exists()
}

pub fn run(t: &Theme) -> i32 {
    println!("{}", t.section("pv doctor — environment preflight"));
    println!(
        " {}  {}  {}  PURPOSE",
        t.table_header(&t.cell("TOOL", 12)),
        t.table_header(&t.cell("STATUS", 10)),
        t.table_header(&t.cell("TIER", 4)),
    );
    let mut missing_required = 0;
    let mut missing_optional = 0;
    for p in PROBES {
        let ok = on_path(p.tool);
        if !ok && p.required {
            missing_required += 1;
        } else if !ok {
            missing_optional += 1;
        }
        let need = if p.required { "core" } else { "opt " };
        println!(
            " {}  {}  {}  {}",
            t.cell(p.tool, 12),
            if ok {
                t.green(&t.cell("ready", 10))
            } else if p.required {
                t.red(&t.cell("required", 10))
            } else {
                t.yellow(&t.cell("missing", 10))
            },
            t.dim(&t.cell(need, 4)),
            t.dim(p.used_for)
        );
    }

    println!();
    println!("{}", t.section("kernel capabilities"));
    println!(
        " {}  {}  {}",
        t.cell("cgroup v2", 12),
        if crate::cgroup::available() {
            t.green(&t.cell("ready", 12))
        } else {
            t.yellow(&t.cell("unavailable", 12))
        },
        t.dim("atomic process-tree freeze/thaw")
    );

    println!();
    println!("{}", t.section("configuration"));
    println!(
        " {}  {}  PURPOSE",
        t.table_header(&t.cell("ITEM", 12)),
        t.table_header(&t.cell("STATUS", 12)),
    );
    let groq = crate::groq::api_key().is_some();
    println!(
        " {}  {}  {}",
        t.cell("groq key", 12),
        if groq {
            t.green(&t.cell("ready", 12))
        } else {
            t.yellow(&t.cell("not set", 12))
        },
        t.dim("inference ($GROQ_API_KEY or ~/.config/pv/groq_api_key)")
    );
    for (file, what) in [
        ("hosts.toml", "pv migrate targets"),
        ("policies.toml", "custom pressure policies"),
    ] {
        let ok = config_exists(file);
        println!(
            " {}  {}  {}",
            t.cell(file, 12),
            if ok {
                t.green(&t.cell("ready", 12))
            } else {
                t.dim(&t.cell("not created", 12))
            },
            t.dim(what)
        );
    }

    println!();
    if missing_required > 0 {
        println!(
            "{}",
            t.red(&format!(
                "{missing_required} required tool(s) missing — inference and update will fail"
            ))
        );
        1
    } else if missing_optional > 0 {
        println!(
            "{}",
            t.yellow(&format!(
                "core ready; {missing_optional} optional tool(s) missing (features above degrade gracefully)"
            ))
        );
        0
    } else {
        println!("{}", t.green("all systems nominal — valve seated"));
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_shell_on_path() {
        assert!(on_path("sh"));
    }

    #[test]
    fn missing_tool_is_absent() {
        assert!(!on_path("pv-definitely-not-a-real-tool-9f3k"));
    }

    #[test]
    fn probes_cover_every_required_tool() {
        // required probes must stay aligned with net.rs / update.rs usage
        assert!(PROBES.iter().any(|p| p.tool == "curl" && p.required));
        assert!(PROBES.iter().any(|p| p.tool == "sha256sum" && p.required));
    }
}
