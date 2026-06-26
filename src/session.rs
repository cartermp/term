// Session persistence: snapshots the window/tab/pane structure so it can be
// restored after a `term` update. PTY processes themselves cannot survive a
// process restart (zsh, vim, Claude Code, etc. all die when their PTY master
// closes); we replay only the layout + CWD per pane, and the new windows open
// fresh shells.
//
// Saved file lives at `~/Library/Application Support/term/session.json`.
// Save is atomic (write-then-rename) so a crash mid-write can't corrupt the
// file.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Format version. Bump on incompatible schema changes; load() refuses
/// snapshots with a different version.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SavedSession {
    pub schema: u32,
    /// `CARGO_PKG_VERSION` at save time. Informational; mismatches are fine.
    pub term_version: String,
    /// Unix-epoch milliseconds at save time. Informational; we don't enforce
    /// freshness here — `--restore-session` is the gate.
    pub saved_at_ms: u128,
    pub windows: Vec<SavedWindow>,
    /// Index into `windows` of the window that had focus at save time.
    pub focused_window: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedWindow {
    /// Outer-frame top-left in logical screen coordinates. `None` when winit
    /// couldn't report a position (rare).
    pub outer_x: Option<i32>,
    pub outer_y: Option<i32>,
    /// Inner-size in logical pixels.
    pub inner_w: u32,
    pub inner_h: u32,
    pub split: Option<SavedSplit>,
    pub panes: Vec<SavedPane>,
    pub active_pane: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SavedSplit {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SavedPane {
    pub cwd: String,
    pub title: String,
}

fn state_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join("Library/Application Support/term"))
}

pub fn state_file() -> Option<PathBuf> {
    Some(state_dir()?.join("session.json"))
}

/// Atomically write `session` to `state_file()`. Returns Ok(()) on success
/// or any I/O error along the way (including missing $HOME).
pub fn save(session: &SavedSession) -> std::io::Result<()> {
    let path = state_file().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no $HOME for session file")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(session)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the session file. Returns `None` if it doesn't exist, can't be
/// parsed, or carries an incompatible `schema` version.
pub fn load() -> Option<SavedSession> {
    let path = state_file()?;
    let data = std::fs::read(&path).ok()?;
    let session: SavedSession = serde_json::from_slice(&data).ok()?;
    if session.schema != SCHEMA_VERSION {
        return None;
    }
    Some(session)
}

/// Best-effort delete. Used after a successful restore so a stale file
/// doesn't auto-restore again next launch.
pub fn clear() {
    if let Some(path) = state_file() {
        let _ = std::fs::remove_file(path);
    }
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let s = SavedSession {
            schema: SCHEMA_VERSION,
            term_version: "1.6.0".to_string(),
            saved_at_ms: 12345,
            focused_window: Some(0),
            windows: vec![SavedWindow {
                outer_x: Some(100),
                outer_y: Some(200),
                inner_w: 960,
                inner_h: 640,
                split: Some(SavedSplit::Vertical),
                active_pane: 1,
                panes: vec![
                    SavedPane {
                        cwd: "/Users/alice".to_string(),
                        title: "alice".to_string(),
                    },
                    SavedPane {
                        cwd: "/tmp".to_string(),
                        title: "tmp".to_string(),
                    },
                ],
            }],
        };

        let json = serde_json::to_string(&s).unwrap();
        let back: SavedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn rejects_incompatible_schema() {
        // hand-craft a JSON with an older schema number; load should treat
        // it as missing rather than try to deserialise mismatched fields.
        let s = SavedSession {
            schema: SCHEMA_VERSION + 99,
            ..Default::default()
        };
        let json = serde_json::to_vec(&s).unwrap();
        let parsed: SavedSession = serde_json::from_slice(&json).unwrap();
        assert_ne!(parsed.schema, SCHEMA_VERSION);
    }

    #[test]
    fn split_serialises_as_lowercase_string() {
        let s = SavedSplit::Horizontal;
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"horizontal\"");
    }
}
