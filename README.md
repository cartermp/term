# term

<img src="assets/icon.svg" width="96" align="right" alt="term icon">

A personal Mac terminal emulator built for terminal-based AI work. Written in Rust, GPU-accelerated via Metal (wgpu), with just enough features to get the job done and nothing more.

## Build

```sh
cargo build --release
cargo run --release
```

Builds four binaries: `term`, `tcat`, `tdiff`, `tjson`. No external tools or scripts needed.

## Features

- **GPU-accelerated rendering** — wgpu/Metal pipeline with batched instancing and a 1024×1024 glyph atlas
- **JetBrains Mono** — bundled font, no system font dependency
- **Catppuccin Mocha** color theme throughout
- **True-color support** — ANSI 8/16, 256-color, and 24-bit RGB
- **Multiple tabs** — Cmd+T/W to open/close, Cmd+[/] or Cmd+1–9 to navigate, drag to reorder
- **Scrollback** — 10,000-line buffer; scroll with mouse wheel, Cmd+Up/Down, Cmd+Home/End
- **Clipboard** — Cmd+C copies selection (text), Cmd+V pastes (text or image path); OSC 52 supported
- **URL detection** — hold Cmd to underline URLs; Cmd+click opens in browser
- **Inline history** — ghost-text completion from `~/.zsh_history`; accept with Cmd+Right or →
- **Alternate screen buffer** — vim, htop, etc. work correctly with `?1049h`
- **Block characters** — 40+ Unicode block/Braille chars rendered as precise fill rectangles
- **Syntax-highlighted `cat`** — `cat` is aliased to `tcat`, which highlights files via `syntect`
- **Syntax-highlighted diffs** — `tdiff` is set as `GIT_PAGER`, so `git diff`, `git show`, and `git log -p` all render with color and line-level highlights
- **JSON prettifier** — `json pnpm dev` (or any command) runs it in a PTY so it sees a real terminal, then pretty-prints any JSON log lines while passing everything else through
- **Shell integration** — ZLE hooks report the input buffer and cursor position live; `chpwd` reports the working directory for dynamic tab titles
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

- No mouse reporting protocols (programs can't receive click/drag events)
- No sixel or kitty image protocols
- No search in scrollback
- No split panes
- No custom keybinding config
- No ligatures or double-width characters
- Bold uses the same font face (no separate bold variant loaded)
