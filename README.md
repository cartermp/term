# term

<img src="assets/icon.svg" width="96" align="right" alt="term icon">

A personal Mac terminal emulator built for terminal-based AI work. Written in Rust, GPU-accelerated via Metal (wgpu), with just enough features to get the job done and nothing more.

## Install

### Pre-built (recommended)

```sh
curl -fsSL https://github.com/cartermp/term/releases/latest/download/install.sh | bash
```

Downloads the latest `Term.app` from GitHub Releases, installs it to `/Applications`, and symlinks `term`, `tcat`, `tdiff`, and `tjson` into `/usr/local/bin`. Pin a version with `TERM_VERSION=v1.0.0 curl ...`.

### From source

Requires Rust (install via [rustup](https://rustup.rs)):

```sh
git clone https://github.com/cartermp/term
cd term
./install.sh
```

Builds a release binary, assembles `Term.app` (including generating `AppIcon.icns` from `assets/icon.svg`), installs it to `/Applications`, and symlinks `term` into `/usr/local/bin`.

## Build

```sh
cargo build --release
cargo run --release
```

Builds four binaries: `term`, `tcat`, `tdiff`, `tjson`. No external tools or scripts needed.

## Testing

```sh
cargo build --all-targets
cargo test
./scripts/manual-smoke.sh
```

Automated coverage includes inline unit tests, subprocess integration tests for `tcat` / `tdiff` / `tjson`, and property-style terminal tests that stress arbitrary byte streams plus mixed write/resize/scroll action sequences.

`./scripts/manual-smoke.sh` creates a temporary fixture workspace with a sample Rust file, JSON fixture, URL fixture, and git diff, prints a macOS smoke checklist, then launches `term` in that workspace. Set `TERM_SMOKE_NO_LAUNCH=1` if you only want the workspace and checklist.

## Features

- **GPU-accelerated rendering** — wgpu/Metal pipeline with batched instancing, static-frame reuse, and a multi-layer glyph atlas
- **JetBrains Mono** — bundled font, no system font dependency
- **Catppuccin Mocha** color theme throughout
- **True-color support** — ANSI 8/16, 256-color, and 24-bit RGB
- **Multiple tabs** — Cmd+T/W to open/close, Cmd+[/] or Cmd+1–9 to navigate, drag to reorder
- **Native self-updates** — `Term -> Check for Updates...` verifies the release SHA-256, bundle identity/version, and code-signature structure before a staged install with rollback
- **Native background controls** — `Appearance -> Background...` opens the macOS color panel with an alpha slider for live background color/opacity changes
- **Scrollback** — 10,000-line buffer; scroll with mouse wheel, Cmd+Up/Down, Cmd+Home/End
- **Resize reflow** — soft-wrapped logical lines reflow when the terminal width changes; copied text preserves hard line breaks
- **Clipboard** — Cmd+C copies selection (text), Cmd+V pastes (text or image path); OSC 52 supported
- **URL detection** — hold Cmd to underline URLs; Cmd+click opens in browser
- **Inline history** — ghost-text completion from `~/.zsh_history`; accept with Cmd+Right or →
- **Alternate screen buffer** — vim, htop, etc. work correctly with `?1049h`
- **Block characters** — 40+ Unicode block/Braille chars rendered as precise fill rectangles
- **Syntax-highlighted `cat`** — `cat` is aliased to `tcat`, which highlights files via `syntect`
- **Syntax-highlighted diffs** — `tdiff` is set as `GIT_PAGER`, so `git diff`, `git show`, and `git log -p` all render with color and line-level highlights
- **JSON prettifier** — `json pnpm dev` (or any command) runs it in a PTY so it sees a real terminal, then pretty-prints any JSON log lines while passing everything else through
- **Shell integration** — ZLE hooks report the input buffer and cursor position live; `chpwd` reports the working directory for dynamic tab titles; mistyped commands show the top 3 likely matches
- **Blinking cursor** — narrow 2px vertical bar, blinks at ~530 ms, resets on input
- **zsh** with your real `~/.zshrc` and `~/.zshenv` sourced automatically

## Keyboard shortcuts

| Shortcut | Action |
|----------|--------|
| Cmd+T | New tab |
| Cmd+W | Close tab |
| Cmd+[ / Cmd+] | Previous / next tab |
| Cmd+1…9 | Jump to tab N |
| Cmd+C | Copy selection |
| Cmd+V | Paste |
| Cmd+Up / Cmd+Down | Scroll one page |
| Cmd+Home / Cmd+End | Scroll to top / bottom |
| Cmd+Left / Cmd+Right | Move to start / end of line |
| Cmd+Backspace | Kill line backward |
| Alt+Left / Alt+Right | Previous / next word |
| Alt+Backspace / Alt+Delete | Kill word backward / forward |
| Cmd+click | Open URL under cursor |

## Utility tools

### `tcat` — syntax-highlighted file viewer

```sh
tcat src/main.rs          # whole file
tcat src/main.rs:40-70    # lines 40–70
tcat src/main.rs:42       # single line
```

Shows a header with file name, language, and directory. The `cat` alias in `term`'s shell uses `tcat` automatically for single-file invocations.

### `tdiff` — syntax-highlighted diff

```sh
git diff          # uses tdiff automatically via GIT_PAGER
git show HEAD
git log -p
```

Highlights added/removed lines with green/red tints and syntax-colors the code content.

### `tjson` — streaming JSON prettifier

```sh
json pnpm dev             # PTY mode: pnpm dev sees a real terminal
json node server.js       # any command that emits JSON log lines
some-cmd | json           # filter mode: reads stdin
```

Lines that parse as JSON objects or arrays are pretty-printed with syntax color. All other output passes through unchanged. PTY mode is preferred for servers (like Next.js) that suppress their startup output when stdout is not a terminal.

## Known gaps

- No sixel or kitty image protocols
- No search in scrollback
- No custom keybinding config
- No publisher-identity guarantee for self-updates until release builds use a private Apple signing identity (release digests and signature structure are still verified)
- Grapheme handling covers combining marks, emoji modifiers, flags, and double-width cells, but is not yet a full Unicode grapheme-break implementation
- Bold uses the same font face (no separate bold variant loaded)
