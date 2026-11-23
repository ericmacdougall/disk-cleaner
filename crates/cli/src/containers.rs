//! The container runtime query (design: Uninspectable processes, container processes). When a
//! runtime child is running (`containerd-shim*`, `conmon`, `runc`, `com.docker.*`), the running
//! containers and their bind sources are listed with `docker` and `podman` and passed to the
//! plan like the process snapshot. Read-only: `ps`, `version` and `inspect` only.

use disk_cleaner_core::evidence::{is_runtime_child, parse_inspect, Container, ContainerState};
use disk_cleaner_core::procs::run_bounded;
use disk_cleaner_core::{ProcKind, ProcessSnapshot};
use std::process::Command;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(30);

/// Query the runtime when one of its processes runs.
pub fn query(snap: &ProcessSnapshot) -> ContainerState {
    let running = snap
        .procs
        .iter()
        .filter(|p| p.kind == ProcKind::Live)
        .any(|p| p.names.iter().any(|n| is_runtime_child(n)));
    if !running {
        return ContainerState::NotQueried;
    }
    let mut errors: Vec<String> = Vec::new();
    let mut all: Vec<Container> = Vec::new();
    let mut answered = false;
    for tool in ["docker", "podman"] {
        match list(tool) {
            Ok(found) => {
                answered = true;
                all.extend(found);
            }
            Err(e) => errors.push(format!("{tool}: {e}")),
        }
    }
    if answered {
        ContainerState::Queried { containers: all }
    } else {
        ContainerState::Unqueryable {
            reason: errors.join("; "),
        }
    }
}

fn list(tool: &str) -> Result<Vec<Container>, String> {
    // The daemon must answer: `version` prints the server version only when it does.
    let server = run_bounded(
        Command::new(tool).args(["version", "--format", "{{.Server.Version}}"]),
        LIMIT,
    )
    .unwrap_or_default();
    if server.trim().is_empty() {
        return Err("the runtime did not answer".to_string());
    }
    let ids = run_bounded(Command::new(tool).args(["ps", "-q", "--no-trunc"]), LIMIT)
        .ok_or_else(|| "could not run `ps`".to_string())?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new(tool);
    cmd.arg("inspect").args(&ids);
    let out = run_bounded(&mut cmd, LIMIT).ok_or_else(|| "could not run `inspect`".to_string())?;
    parse_inspect(&out)
}
