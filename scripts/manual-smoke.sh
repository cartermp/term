#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="${TMPDIR:-/tmp}"
WORKDIR="${TERM_SMOKE_DIR:-$(mktemp -d "${TMP_ROOT%/}/term-smoke.XXXXXX")}"

mkdir -p "$WORKDIR"

cat > "$WORKDIR/sample.rs" <<'EOF'
fn greet(name: &str) -> String {
    format!("hello, {name}")
}

fn main() {
    println!("{}", greet("term"));
}
EOF

cat > "$WORKDIR/sample.json" <<'EOF'
{"service":"term","ok":true,"count":3,"items":["tabs","colors","json"]}
EOF

cat > "$WORKDIR/url.txt" <<'EOF'
Cmd-click this URL inside term: https://example.com/docs/term?smoke=1
EOF

cat > "$WORKDIR/emit-json.sh" <<'EOF'
#!/bin/sh
printf '%s\n' 'server booting'
printf '%s\n' '{"mode":"pty","service":"term","ok":true,"count":3}'
printf '%s\n' 'stderr line from child' >&2
EOF
chmod +x "$WORKDIR/emit-json.sh"

(
    cd "$WORKDIR"
    git init -q
    git config user.name "term smoke"
    git config user.email "term-smoke@example.com"
    git add sample.rs sample.json url.txt emit-json.sh
    git commit -qm "smoke fixtures"
)

cat > "$WORKDIR/sample.rs" <<'EOF'
use std::io::{self, Write};

fn greet(name: &str) -> String {
    format!("hello, {name}")
}

fn main() {
    let mut stdout = io::stdout();
    writeln!(stdout, "{}", greet("term smoke")).unwrap();
}
EOF

cat > "$WORKDIR/notes.txt" <<'EOF'
Use this file for copy/paste and selection checks.
EOF

cat <<EOF
Prepared smoke workspace: $WORKDIR

Run this matrix inside term after the window opens:
  1. Prompt boots in $WORKDIR and the tab title tracks the cwd/command.
  2. cat sample.rs
     - syntax-highlighted Rust output via tcat
  3. git diff
     - syntax-highlighted diff via tdiff
  4. printf '%s\n' '{"hello":"world","ok":true}' | json
     - prettified JSON in filter mode
  5. json ./emit-json.sh
     - PTY mode keeps plain lines, stderr, and pretty-prints the JSON line
  6. echo -e "\e[31mred\e[0m \e[32mgreen\e[0m \e[34mblue\e[0m"
     - ANSI colors render correctly
  7. seq 1 200
     - scrollback works with wheel, Cmd+Up/Down, Cmd+Home/End
  8. vim sample.rs
     - alternate screen, cursor restore, and keyboard input behave correctly
  9. htop  (or top if htop is not installed)
     - full-screen app redraw stays stable
  10. cat url.txt
      - hold Cmd to underline the URL, then Cmd-click to open it
  11. Cmd+T / Cmd+W / Cmd+1
      - tabs open, close, and switch cleanly
  12. Select text from notes.txt output, then Cmd+C / Cmd+V
      - selection and clipboard round-trip correctly

Set TERM_SMOKE_NO_LAUNCH=1 to only prepare this workspace and checklist.
EOF

if [[ "${TERM_SMOKE_NO_LAUNCH:-0}" == "1" ]]; then
    exit 0
fi

cd "$WORKDIR"
cargo run --release --manifest-path "$ROOT/Cargo.toml"
