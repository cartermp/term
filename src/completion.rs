// Ghost-text engine: suggests completions from shell history inline.
// Tab completion is handled natively by zsh — we just pass \t through.

pub struct Engine {
    history: Vec<String>,
}

impl Engine {
    pub fn new() -> Self {
        Self { history: load_history() }
    }

    /// Returns the suffix to append as ghost text, or None.
    pub fn ghost(&self, prefix: &str, cursor_at_end: bool) -> Option<&str> {
        if !cursor_at_end || prefix.trim().is_empty() { return None; }
        self.history.iter()
            .find(|h| h.starts_with(prefix) && h.as_str() != prefix)
            .map(|h| &h[prefix.len()..])
    }
}

fn load_history() -> Vec<String> {
    let path = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h).join(".zsh_history"),
        Err(_) => return vec![],
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let text = String::from_utf8_lossy(&data);

    let mut seen  = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for line in text.lines().rev() {
        // Strip extended history prefix:  `: 1234567890:0;actual command`
        let cmd = if line.starts_with(": ") {
            match line.splitn(3, ';').nth(1) {
                Some(s) => s,
                None    => line,
            }
        } else {
            line
        };
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() { continue; }
        if seen.insert(cmd.clone()) { lines.push(cmd); }
    }
    lines
}
