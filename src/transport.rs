//! A shared musical clock so several autochord instances on the same machine
//! (same user) run their arpeggiators in exact lockstep.
//!
//! The state is tiny — a tempo and a wall-clock "epoch" (the UNIX-millis moment
//! of beat 0) — kept in a per-user file. Every instance derives its step
//! position from `SystemTime::now()` relative to that shared epoch, so they all
//! land on the same grid regardless of process. (A process-local `Instant`
//! couldn't be compared across processes, so we anchor to the wall clock, which
//! every process on the box agrees on.)

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often to re-read the shared file to pick up another instance's changes.
const SYNC_INTERVAL: Duration = Duration::from_millis(200);

pub struct Transport {
    /// Shared state file, or `None` for a disconnected (in-memory) transport.
    path: Option<PathBuf>,
    tempo: u32,
    epoch_ms: u64,
    last_sync: Instant,
}

impl Transport {
    /// Open (or create) the shared per-user transport.
    pub fn new() -> Self {
        let path = shared_path();
        let (tempo, epoch_ms) = read(&path).unwrap_or_else(|| {
            let epoch = now_ms();
            write(&path, 120, epoch); // first instance establishes the grid
            (120, epoch)
        });
        Self {
            path: Some(path),
            tempo,
            epoch_ms,
            last_sync: Instant::now(),
        }
    }

    /// An in-memory transport that never touches the filesystem (for tests).
    #[cfg(test)]
    pub fn disconnected() -> Self {
        Self {
            path: None,
            tempo: 120,
            epoch_ms: now_ms(),
            last_sync: Instant::now(),
        }
    }

    pub fn tempo(&self) -> u32 {
        self.tempo
    }

    /// Position on the shared grid in arp steps (fractional), from the wall
    /// clock relative to the shared epoch.
    pub fn step_position(&self, subdiv: u32) -> f64 {
        let elapsed = now_ms().saturating_sub(self.epoch_ms) as f64;
        elapsed * self.tempo as f64 * subdiv as f64 / 60_000.0
    }

    /// Set the tempo, re-anchoring the epoch so the current beat position is
    /// preserved (no jump), and publish it for the other instances. Preserving
    /// beats keeps every subdivision continuous, whatever each instance uses.
    pub fn set_tempo(&mut self, tempo: u32) {
        let beats = self.step_position(1); // beats elapsed since the epoch
        self.tempo = tempo.max(1);
        let back = (beats * 60_000.0 / self.tempo as f64) as u64;
        self.epoch_ms = now_ms().saturating_sub(back);
        if let Some(path) = &self.path {
            write(path, self.tempo, self.epoch_ms);
        }
    }

    /// Adopt another instance's tempo/epoch, throttled so we don't hammer the
    /// filesystem. Call once per UI frame.
    pub fn sync(&mut self) {
        let Some(path) = &self.path else { return };
        if self.last_sync.elapsed() < SYNC_INTERVAL {
            return;
        }
        self.last_sync = Instant::now();
        if let Some((tempo, epoch)) = read(path) {
            self.tempo = tempo;
            self.epoch_ms = epoch;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn shared_path() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("autochord-transport-{user}"))
}

fn read(path: &PathBuf) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut fields = text.split_whitespace();
    let tempo = fields.next()?.parse().ok()?;
    let epoch = fields.next()?.parse().ok()?;
    Some((tempo, epoch))
}

fn write(path: &PathBuf, tempo: u32, epoch_ms: u64) {
    // Write to a pid-tagged temp file then rename — an atomic swap, so a
    // concurrent reader never sees a half-written line.
    let tmp = path.with_file_name(format!("autochord-transport.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{tempo} {epoch_ms}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
