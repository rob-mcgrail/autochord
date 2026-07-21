//! Text control interface for agents/scripts — no server, no MCP, just files.
//!
//! Everything lives in a shared per-user directory:
//!
//! ```text
//! $TMPDIR/autochord-<user>/
//!   global        # shared clock: `tempo <bpm>` + `epoch_ms <n>` (read-only)
//!   <pid>.state   # a running instance's full state as `key value` lines
//!   <pid>.in      # inbox: write `key value` lines; the instance applies + deletes them
//! ```
//!
//! The state keys and the command keys are the SAME, so reading state tells you
//! exactly what you can write back. The `autochord` binary also exposes
//! `state` / `send` / `ls` subcommands as a thin wrapper over these files.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// How often a running instance rewrites its `.state` file.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(120);
/// Files left by a crashed instance are cleaned up once this stale.
const STALE: Duration = Duration::from_secs(5);

/// The shared per-user control directory.
pub fn dir() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("autochord-{user}"))
}

/// Path of the global shared-clock file (managed by the transport).
pub fn global_path() -> PathBuf {
    dir().join("global")
}

fn state_path(pid: u32) -> PathBuf {
    dir().join(format!("{pid}.state"))
}

fn inbox_path(pid: u32) -> PathBuf {
    dir().join(format!("{pid}.in"))
}

/// A running instance's handle to its own state file and command inbox.
pub struct Control {
    state: PathBuf,
    inbox: PathBuf,
    last_publish: Option<Instant>,
}

impl Control {
    pub fn new() -> Self {
        let d = dir();
        let _ = fs::create_dir_all(&d);
        remove_stale(&d);
        let pid = std::process::id();
        Self {
            state: state_path(pid),
            inbox: inbox_path(pid),
            last_publish: None,
        }
    }

    /// Consume any queued command lines, deleting the inbox. Cheap — call every
    /// frame so commands are picked up promptly.
    pub fn take_commands(&self) -> Vec<String> {
        match fs::read_to_string(&self.inbox) {
            Ok(text) => {
                let _ = fs::remove_file(&self.inbox);
                text.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    }

    /// True when the published state is due for a refresh (throttled).
    pub fn due(&mut self) -> bool {
        let now = Instant::now();
        if self.last_publish.is_none_or(|t| now.duration_since(t) >= PUBLISH_INTERVAL) {
            self.last_publish = Some(now);
            true
        } else {
            false
        }
    }

    pub fn publish(&self, state: &str) {
        let _ = fs::write(&self.state, state);
    }

    /// Remove this instance's files on exit.
    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.state);
        let _ = fs::remove_file(&self.inbox);
    }
}

/// Remove `.state` / `.in` files left behind by dead instances.
fn remove_stale(dir: &PathBuf) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "state" || e == "in");
        if !ours {
            continue;
        }
        if let Ok(age) = entry.metadata().and_then(|m| m.modified()).map(|t| now.duration_since(t)) {
            if age.map(|a| a > STALE).unwrap_or(false) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

// --- CLI helpers (used by the `autochord` subcommands) -----------------------

/// PIDs of instances with a live `.state` file, ascending.
pub fn live_instances() -> Vec<u32> {
    let mut pids = Vec::new();
    if let Ok(entries) = fs::read_dir(dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("state") {
                if let Some(pid) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
                    pids.push(pid);
                }
            }
        }
    }
    pids.sort_unstable();
    pids
}

/// Render `autochord state [pid]` output: the global block, then the requested
/// instance (or all instances).
pub fn cli_state(pid: Option<u32>) -> String {
    let mut out = String::new();
    if let Ok(global) = fs::read_to_string(global_path()) {
        out.push_str("# global\n");
        out.push_str(&global);
        if !global.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    match pid {
        Some(pid) => match fs::read_to_string(state_path(pid)) {
            Ok(s) => out.push_str(&s),
            Err(_) => out.push_str(&format!("# no instance {pid}\n")),
        },
        None => {
            let pids = live_instances();
            if pids.is_empty() {
                out.push_str("# no running instances\n");
            }
            for pid in pids {
                if let Ok(s) = fs::read_to_string(state_path(pid)) {
                    out.push_str(&s);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Append a command line to an instance's inbox (`autochord send <pid> ...`).
pub fn cli_send(pid: u32, command: &str) -> std::io::Result<()> {
    use std::io::Write;
    let _ = fs::create_dir_all(dir());
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path(pid))?;
    writeln!(file, "{command}")
}
