# AGENTS.md

## Project

`term` — a Mac-native terminal emulator in Rust. Runs zsh inside a PTY, parses VT/ANSI escape codes, and renders to a GPU-accelerated Metal surface (wgpu) using the Catppuccin Mocha color theme.

Four binaries are built by `cargo build`:
- `term` — the terminal emulator
- `tcat` — syntax-highlighted file viewer
- `tdiff` — syntax-highlighted diff pager
- `tjson` — streaming JSON prettifier (filter or PTY mode)

## Build

```sh
cargo build           # debug build
cargo build --release # release build (use for performance work)
cargo test            # run all tests
```

Dependencies are fetched automatically by cargo. No other setup required.

## Testing

After any change:

1. `cargo build` must succeed with zero errors and zero warnings.
2. `cargo test` must pass.
3. `cargo run --release` must open a window and display a working zsh prompt.
4. Manually verify: type commands, run `vim`, run `htop`, check colors with `echo -e "\e[31mred\e[0m"`.

Unit tests live inline in each source file. Run a specific module with e.g. `cargo test --lib terminal`.

## Code map

```
src/
  config.rs         Color constants (Catppuccin Mocha), ansi_256_color(), FONT_SIZE_PT
  terminal.rs       VT state machine (vte::Perform impl), Cell/Attrs types, TerminalState,
                    Terminal wrapper, scrollback (VecDeque), alternate screen buffer
  renderer.rs       wgpu pipelines (rect + glyph), 1024×1024 glyph atlas, block-char rendering,
                    tab bar, URL underlines, selection highlight, cursor bar
  main.rs           winit event loop, wgpu surface init, PTY spawn per tab, keyboard routing,
                    tabs (Vec<Tab>), clipboard (pbcopy/pbpaste/OSC52), URL detection,
                    ghost text (zsh_history), ZDOTDIR shell injection, mouse handling
  bin/tcat.rs       standalone syntax-highlighting cat (syntect, base16-ocean.dark)
  bin/tdiff.rs      unified diff processor; used as GIT_PAGER
  bin/tjson.rs      JSON prettifier; filter mode (stdin) and PTY mode (spawns command)
assets/
  JetBrainsMono-Regular.ttf   bundled font (loaded via include_bytes!)
```

## Key types

| Type | File | Purpose |
|------|------|---------|
| `Color` | config.rs | RGB triple; theme constants |
| `Attrs` | terminal.rs | Per-cell style: fg/bg colors + bold/italic/underline/inverse |
| `Cell` | terminal.rs | Single terminal cell: `char` + `Attrs` |
| `TerminalState` | terminal.rs | Grid, scrollback, cursor, scroll region, alt screen, VT performer |
| `Terminal` | terminal.rs | Wraps `vte::Parser` + `TerminalState`; exposes `process(&[u8])` |
| `Renderer` | renderer.rs | wgpu device/queue, two render pipelines, glyph atlas, `render()` |
| `Tab` | main.rs | PTY pair + `Terminal` instance; one per tab |
| `App` | main.rs | winit `ApplicationHandler`; owns Vec<Tab>, Renderer, selection, ghost text |
| `AppEvent` | main.rs | `PtyData { tab_id, data }` / `PtyExit { tab_id }` sent via `EventLoopProxy` |

## Threading model

- **Main thread**: winit event loop, rendering, keyboard input, PTY writes.
- **Reader thread** (one per tab): blocking `Read` on PTY master; sends `AppEvent::PtyData` via `EventLoopProxy`. Never touches the terminal grid directly.

Do not add shared mutable state across threads. Route all PTY output through `AppEvent`.

## Renderer architecture

Two wgpu pipelines, both using instanced drawing:

**Rect pipeline** (`RectInst { pos, sz, color }`)
- Used for: cell backgrounds, selection highlight, cursor bar, block characters, URL underlines, tab bar backgrounds.

**Glyph pipeline** (`GlyphInst { pos, sz, uv_pos, uv_sz, fg }`)
- Glyph bitmaps are rasterized by fontdue and packed into a 1024×1024 R8 atlas texture.
- Cache key is `char` only (bold shares the same entry).
- Atlas evicts and restarts when full (rare; terminals use a small glyph set).

Block/Braille characters (U+2580–U+259F and U+2800–U+28FF) are rendered as fill rects, not atlas lookups.

Each `render()` call fills `Vec<RectInst>` and `Vec<GlyphInst>`, uploads them to GPU vertex buffers (grown on demand), and issues two draw calls.

## Adding escape sequences

Implement in `TerminalState`'s `Perform` methods in `src/terminal.rs`:

- Printable characters → `fn print`
- C0 controls (BS, LF, CR, TAB) → `fn execute`
- CSI sequences → `fn csi_dispatch` — dispatch on `(intermediates.first().copied().unwrap_or(0), action as u8)`
- OSC sequences → `fn osc_dispatch` — params are `&[&[u8]]`
- ESC sequences → `fn esc_dispatch`

To send a response back to the PTY (e.g. cursor position report, OSC 52 reply), push to `self.pending_responses`; `main.rs` drains and writes them after each `process()` call.

## Tabs

`App` owns `Vec<Tab>` and `active_tab: usize`. Each `Tab` has its own PTY reader thread. Opening a tab spawns a new zsh process with the same `setup_shell_env` call. Closing a tab drops the PTY writer (sending EOF to zsh) and removes the entry from the vec.

## Scrollback

`TerminalState.scrollback: VecDeque<Vec<Cell>>`, capped at `SCROLLBACK_MAX = 10_000`. Lines are pushed there when they scroll off the top of the live grid (normal screen only; alternate screen is excluded). `viewport_offset` (0 = live) shifts `visual_cell()` lookups into scrollback. The renderer and selection code both use `visual_cell()` so they automatically reflect the scrolled view.

## Shell environment

`setup_shell_env(&mut CommandBuilder)` in `main.rs`:
1. Creates a temp `ZDOTDIR` (`/tmp/term_zsh_{pid}/`) with `.zshenv` and `.zshrc`.
2. `.zshrc` sources the user's real config, then defines `cat` → `tcat`, `json` → `tjson "$@"`, sets `GIT_PAGER=tdiff`, installs ZLE + chpwd + precmd/preexec hooks.
3. Sets `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=ghostty`.
4. Sets `LANG=en_US.UTF-8` and `LC_ALL=en_US.UTF-8` if not already in the environment. This is required when `term` is launched from Finder/launchd (no inherited locale); without it, macOS defaults to Mac Roman and zsh re-encodes UTF-8 bytes as Mac Roman–decoded Unicode, producing garbled multi-byte characters.

## `tjson` PTY mode

`tjson` (and the `json` shell alias) accepts an optional command as arguments:

```sh
json pnpm dev        # PTY mode: spawns pnpm dev in a PTY
pnpm dev | json      # filter mode: reads from stdin
```

PTY mode is necessary for commands like Next.js dev servers that detect whether stdout is a terminal and suppress their formatted startup output if it isn't. `portable_pty` is reused here (same dependency as the main terminal).

`run_pty` inherits `COLUMNS`/`LINES` from the environment for PTY sizing (defaults to 220×50). The child inherits the full shell environment, including `LANG`.

## Changing the color theme

All colors live in `src/config.rs`:
- `DEFAULT_FG` / `DEFAULT_BG` — base foreground/background
- `CURSOR_COLOR` — cursor bar color
- `ANSI_COLORS: [Color; 16]` — standard 16-color palette
- `ansi_256_color(index)` — 256-color + grayscale ramp

## Changing font size

Edit `FONT_SIZE_PT` in `src/config.rs`. The renderer scales by the window's DPI factor.

## Common pitfalls

- **Scroll region**: `scroll_up`/`scroll_down` operate on the live `grid` vec using index arithmetic. After `remove(scroll_top)`, insert at `scroll_bottom` (not `scroll_bottom - 1`) — the removal already shifted indices.
- **Glyph baseline**: `gy0 = py + baseline - (ymin + height)`. `ymin` in fontdue is the bottom of the bounding box relative to baseline. Getting this wrong causes glyphs to float or clip.
- **DPI**: `Renderer` stores physical-pixel sizes. Window resize events from winit give physical pixels. `LogicalSize` is only used for the initial window creation.
- **PTY slave lifetime**: drop `pair.slave` after `spawn_command` — keeping it open prevents the reader thread from ever seeing EOF when the shell exits.
- **Alternate screen**: `alt_grid` and `alt_saved_cursor` are separate from the normal grid. Operations on the live grid (scrollback push, viewport snap) must check `alt_screen` and skip when true.
- **tjson cwd**: `CommandBuilder::new()` defaults to the process's home directory, not `current_dir()`. Always call `cmd.cwd(std::env::current_dir()?)` so the spawned command runs in the same directory as the shell invoking `tjson`.
- **UTF-8 in tjson**: `BufReader::lines()` requires valid UTF-8. If the PTY output contains raw C1 bytes (0x80–0x9F) that aren't part of a multi-byte sequence, `lines()` will error and the loop will break. Be aware of this if expanding `run_pty`.
- **GPU buffer growth**: `rect_buf` and `glyph_buf` are grown on demand by reallocating. The capacity is tracked in `rect_buf_cap` / `glyph_buf_cap`. Don't assume a fixed size.
