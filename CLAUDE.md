# term

Mac-native terminal emulator written in Rust. Runs zsh, renders via a GPU-accelerated Metal pipeline (wgpu), rasterizes glyphs with fontdue. Catppuccin Mocha color theme.

## Build & run

```sh
cargo build                  # debug
cargo build --release        # optimized (preferred for real use)
cargo run --release
```

No external tools, no scripts. The standard cargo workflow is the whole story.

## Architecture

| File | Responsibility |
|------|---------------|
| `src/config.rs` | Color theme (Catppuccin Mocha), `ansi_256_color`, font size constant |
| `src/terminal.rs` | VT/ANSI state machine (`vte::Perform`), terminal grid, scrollback, `Terminal` wrapper |
| `src/renderer.rs` | wgpu pipelines, glyph atlas, per-frame GPU render |
| `src/main.rs` | winit 0.30 event loop, wgpu surface, PTY lifecycle, tabs, keyboard input, clipboard, URL detection, ghost text |

### Data flow

```
zsh (PTY slave)
  └─ PTY master → reader thread
                      └─ EventLoopProxy::send_event(AppEvent::PtyData)
                                └─ terminal.process(bytes)   ← vte parser
                                       └─ window.request_redraw()
                                              └─ renderer.render() → wgpu surface
```

Keyboard input goes the other direction: `winit KeyboardInput → handle_key → pty_writer`.

### Terminal emulation

`TerminalState` owns the grid (`Vec<Vec<Cell>>`) and implements `vte::Perform`. Supported:
- SGR colors: standard ANSI 8/16, 256-color (`38;5;n`), true-color (`38;2;r;g;b`)
- Cursor movement: CUP, CUU/D/F/B, CHA, VPA, home/end, cursor save/restore (ESC 7/8 and CSI s/u)
- Erase: ED (0/1/2/3), EL (0/1/2), ECH
- Scroll region: DECSTBM (`r`), scroll up/down (SU/SD, IL/DL)
- Insert/delete: ICH (`@`), DCH (`P`), IL (`L`), DL (`M`)
- Device attributes: `ESC[c` → `\x1b[?1;2c`
- Device status report: `ESC[6n` → cursor position reply
- OSC 0/2: window title
- OSC 7: working directory (`file://hostname/path`) → used for tab titles
- OSC 52: clipboard read/write (base64)
- OSC 9001: ZLE buffer + cursor position (shell integration)
- Private modes: `?47h`/`?1047h`/`?1049h` (alternate screen), `?2004h` (bracketed paste)

### Scrollback

`TerminalState.scrollback` is a `VecDeque<Vec<Cell>>` capped at `SCROLLBACK_MAX = 10_000` lines. Lines pushed off the top of the live grid are appended there. Alternate-screen content is not captured. `viewport_offset` (0 = live view) controls what `visual_cell()` returns for rendering and selection.

### Alternate screen buffer

`?1049h` saves the cursor and switches to `alt_grid`. `?1049l` restores. `?47h`/`?1047h` switch without cursor save. vim, htop, etc. work via this mechanism.

### Rendering

`Renderer` maintains two wgpu render pipelines:
- **Rect pipeline** — fills solid color rectangles (backgrounds, block chars, cursor, selection, URL underlines)
- **Glyph pipeline** — draws text from a 1024×1024 R8 atlas texture using instanced quads

Each frame: collect `RectInst` and `GlyphInst` vecs from the terminal grid, upload to GPU buffers, issue two draw calls. Block/Braille characters (U+2580–U+259F and Braille range) are decomposed into fill rects rather than looked up in the font.

Glyph cache key is `char` only (bold uses the same rasterization). Cache is never evicted unless the atlas overflows, at which point it is fully cleared and rebuilt.

### Cursor

A narrow 2-physical-pixel vertical bar drawn on top of the current cell after the full cell pass. Color: `CURSOR_COLOR`. Blinks at ~530 ms on/off via `about_to_wait` + `ControlFlow::WaitUntil`. Blink resets (cursor always shown) on keypress or PTY output.

## Syntax highlighting (`tcat`, `tdiff`, `tjson`)

### `tcat`

`src/bin/tcat.rs` reads a file, applies 24-bit true-color ANSI syntax highlighting via `syntect` (theme: `base16-ocean.dark`), and prints to stdout with a header and line numbers.

```sh
tcat file.rs          # whole file
tcat file.rs:40-70    # line range
tcat file.rs:42       # single line
```

### `tdiff`

`src/bin/tdiff.rs` reads a unified diff from stdin, applies syntax highlighting to the code content, and renders added/removed lines with green/red background tints (Catppuccin Mocha palette). Used as `GIT_PAGER` so `git diff`, `git show`, `git log -p`, etc. automatically render with syntax colors.

### `tjson`

`src/bin/tjson.rs` is a streaming JSON prettifier with two modes:

**Filter mode** (stdin pipe):
```sh
some-cmd | json
```

**PTY mode** (recommended for servers):
```sh
json pnpm dev
json node server.js
```

PTY mode spawns the command inside a pseudo-terminal so the child process sees a real terminal on stdout (enabling its full formatted output), then filters the combined output line by line. Lines starting with `{` or `[` that parse as valid JSON are pretty-printed with syntax highlighting; all other lines pass through unchanged.

### Shell init

On startup, `term` writes a ZDOTDIR-based zsh init that:
1. Sources the user's real `~/.zshenv`, `~/.zprofile`, and `~/.zshrc`
2. Defines `function cat()` that calls `tcat` for single-file invocations
3. Defines `function json()` that calls `tjson "$@"` (passes all args)
4. Sets `GIT_PAGER=tdiff` and `GIT_COLOR_UI=never` so git hands raw diff to `tdiff`
5. Installs ZLE hooks (`add-zle-hook-widget`) for live buffer reporting (OSC 9001)
6. Installs `chpwd`, `precmd`, and `preexec` hooks for dynamic tab titles (OSC 0) and working directory (OSC 7)
7. Sets `LANG=en_US.UTF-8` / `LC_ALL=en_US.UTF-8` if not already set (prevents Mac Roman re-encoding of UTF-8 when `term` is launched without a locale from Finder/launchd)

All four binaries (`term`, `tcat`, `tdiff`, `tjson`) live next to each other in the build output directory. `cargo build` builds all four.

## Tabs

Multiple tabs are supported. Each tab owns its own `Terminal` and PTY pair.

| Key | Action |
|-----|--------|
| Cmd+T | New tab |
| Cmd+W | Close tab |
| Cmd+[ / Cmd+] | Previous / next tab |
| Cmd+1…9 | Jump to tab N |

Tab titles update dynamically: CWD at the prompt, command name while a command is running (via OSC 0 from ZLE hooks).

Tabs can be reordered by left-click-drag on the tab bar.

## Clipboard

- **Copy** (Cmd+C): extracts selected text from the terminal grid, strips `tcat` line-number gutter characters, and pipes to `pbcopy`.
- **Paste** (Cmd+V): checks clipboard for a PNG image first (saves to a temp file and writes the path to the PTY); falls back to text via `pbpaste`. Wraps in `\x1b[200~`/`\x1b[201~` when bracketed paste mode is active.
- **OSC 52**: applications can query or set the clipboard via base64-encoded escape sequences.

## URL detection

URLs (`http://` and `https://`) in the visible terminal rows are detected each frame. When the Cmd key is held:
- Detected URLs are underlined.
- The cursor changes to a pointer over a URL.
- Cmd+click launches the URL with `open`.

Trailing punctuation (`.,:;)`) is stripped from detected URLs.

## Ghost text (inline history)

When typing at an empty-or-non-empty prompt with the cursor at the end, `term` scans `~/.zsh_history` for the most recent command with the current buffer as a prefix and renders the remainder in dim gray as ghost text. Accept with Cmd+Right or →; any other key clears it.

## Crate versions (pinned in Cargo.lock)

- `winit 0.30` — windowing, `ApplicationHandler` API
- `wgpu 0.20` — GPU render pipeline (Metal backend on macOS)
- `pollster 0.3` — block-on for wgpu async init
- `bytemuck 1` — safe casting for GPU instance data
- `vte 0.13` — VT/ANSI parser
- `portable-pty 0.8` — PTY open + zsh spawn
- `fontdue 0.8` — pure-Rust glyph rasterizer
- `syntect 5` — syntax highlighting (features: `default-syntaxes`, `default-themes`, `parsing`, `regex-fancy`)
- `serde_json 1` — JSON parsing for `tjson`
- `objc2 0.5` — macOS clipboard (PNG detection)

## Font

JetBrains Mono Regular is bundled in `assets/JetBrainsMono-Regular.ttf` and loaded at startup via `include_bytes!`. No system font is required. To change the font size, edit `FONT_SIZE_PT` in `src/config.rs`.

## Testing

```sh
cargo test          # all tests
cargo test --lib    # terminal + renderer unit tests only
```

Tests live inline in `src/terminal.rs` (VT sequences, scrollback, SGR), `src/bin/tcat.rs` (header/range rendering), `src/bin/tdiff.rs` (diff parsing), and `src/bin/tjson.rs` (JSON detection, passthrough, ANSI output).

## Known gaps / future work

- No mouse reporting protocols (programs can't receive click/drag events)
- No sixel or kitty image protocols
- No search in scrollback
- No split panes
- No ligatures or double-width characters
- No custom keybinding or theme config files
- Bold uses the same font face (no separate bold variant loaded)
