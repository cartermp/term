# AGENTS.md

## Project

`term` — a Mac-native terminal emulator in Rust. Runs zsh inside a PTY, parses VT/ANSI escape codes, and renders to a GPU-backed framebuffer using the Catppuccin Mocha color theme.

## Build

```sh
cargo build           # debug build
cargo build --release # release build (use for performance work)
```

Dependencies are fetched automatically by cargo. No other setup required.

## Testing

There is no test suite yet. After any change:

1. `cargo build` must succeed with zero errors and zero warnings.
2. `cargo run` (or `cargo run --release`) must open a window and display a working zsh prompt.
3. Manually verify: type commands, run `vim`, run `htop`, check colors with `echo -e "\e[31mred\e[0m"`.

## Code map

```
src/
  config.rs       Color constants (Catppuccin Mocha), ansi_256_color(), FONT_SIZE_PT
  terminal.rs     VT state machine (vte::Perform impl), Cell/Attrs types, Terminal wrapper
  renderer.rs     fontdue glyph cache, frame rendering to &mut [u32], narrow cursor bar
  main.rs         winit event loop, softbuffer surface, PTY spawn, keyboard routing,
                  cursor blink (about_to_wait), ZDOTDIR shell injection
  bin/tcat.rs     standalone syntax-highlighting cat (syntect, base16-ocean.dark theme)
```

## Key types

| Type | File | Purpose |
|------|------|---------|
| `Color` | config.rs | RGB triple; `to_u32()` and `blend()` helpers |
| `Attrs` | terminal.rs | Per-cell style: fg/bg colors + bold/italic/underline/inverse |
| `Cell` | terminal.rs | Single terminal cell: `char` + `Attrs` |
| `TerminalState` | terminal.rs | Grid (`Vec<Vec<Cell>>`), cursor, scroll region, VT performer |
| `Terminal` | terminal.rs | Wraps `vte::Parser` + `TerminalState`; exposes `process(&[u8])` |
| `Renderer` | renderer.rs | fontdue font, glyph cache, `render()` blit |
| `App` | main.rs | winit `ApplicationHandler`, owns PTY master + writer + Terminal + Renderer |
| `AppEvent` | main.rs | `PtyData(Vec<u8>)` / `PtyExit` sent across threads via `EventLoopProxy` |

## Threading model

- **Main thread**: winit event loop, rendering, keyboard input, PTY writes.
- **Reader thread**: blocking `Read` on PTY master; sends `AppEvent::PtyData` via `EventLoopProxy`. Never touches the terminal grid directly.

Do not add shared mutable state across threads. Route all PTY output through `AppEvent`.

## Adding escape sequences

Implement in `TerminalState`'s `Perform` methods in `src/terminal.rs`:

- Printable characters → `fn print`
- C0 controls (BS, LF, CR, TAB) → `fn execute`
- CSI sequences → `fn csi_dispatch` — dispatch on `(intermediates.first().copied().unwrap_or(0), action)`
- OSC sequences → `fn osc_dispatch`
- ESC sequences → `fn esc_dispatch`

To send a response back to the PTY (e.g. cursor position report), push to `self.pending_responses`; `main.rs` drains and writes them after each `process()` call.

## Changing the color theme

All colors live in `src/config.rs`:
- `DEFAULT_FG` / `DEFAULT_BG` — base foreground/background
- `CURSOR_COLOR` — cursor block color
- `ANSI_COLORS: [Color; 16]` — standard 16-color palette
- `ansi_256_color(index)` — 256-color + grayscale ramp

## Changing font size or font path

- Size: `FONT_SIZE_PT` in `src/config.rs` (points; scaled by DPI in `Renderer::new`).
- Font: `Renderer::load_font()` in `src/renderer.rs` tries a list of paths in order.

## Style rules

- No `unwrap()` in hot paths (the render loop). Prefer `if let` / `match`.
- Keep `TerminalState` free of I/O. All PTY writes go through `pending_responses` or `App::pty_write`.
- The glyph cache (`HashMap<(char, bool), Glyph>`) is append-only. Do not add eviction without profiling first.
- Avoid allocations in `Renderer::render`. The per-frame path should only touch the existing cache and write into the provided `&mut [u32]`.

## Cursor blink

- `App` fields: `cursor_visible: bool`, `last_blink: Instant`
- `about_to_wait` toggles `cursor_visible` every 530 ms and calls `window.request_redraw()`; sets `ControlFlow::WaitUntil(last_blink + 530ms)` so the event loop wakes at the right time
- `reset_blink()` (called on keypress and PTY data) forces the cursor visible and resets the timer — cursor stays solid while active
- `renderer.render(..., cursor_visible)` receives the current state; when `false`, the cursor bar is simply not drawn

## Syntax highlighting (`tcat`)

- `src/bin/tcat.rs` is a separate binary compiled by the same `cargo build`
- `setup_shell_env(&mut cmd)` in `main()` creates a temp ZDOTDIR and writes `.zshenv` + `.zshrc` that source the user's real config then define `function cat()` pointing at `tcat`
- The `tcat` path is `std::env::current_exe().parent().join("tcat")` — works for both debug and release builds since both binaries land in the same directory
- To change the highlight theme, edit the `ts.themes["base16-ocean.dark"]` line in `src/bin/tcat.rs`; available defaults: `base16-ocean.dark`, `base16-ocean.light`, `base16-eighties.dark`, `base16-mocha.dark`, `Solarized (dark)`, `InspiredGitHub`
- `as_24_bit_terminal_escaped(&ranges, false)` — the `false` suppresses background color escapes so the terminal's own background shows through

## Common pitfalls

- **Scroll region**: `scroll_up`/`scroll_down` in `TerminalState` operate on the live `grid` vec using index arithmetic. After `remove(scroll_top)`, insert at `scroll_bottom` (not `scroll_bottom - 1`) — the removal already shifted indices.
- **Glyph baseline**: `gy0 = py + baseline - (ymin + height)`. `ymin` in fontdue is the bottom of the bounding box relative to baseline (positive = above baseline). Getting this wrong causes glyphs to float or clip.
- **DPI**: `Renderer` stores physical-pixel sizes. Window resize events from winit give physical pixels. `LogicalSize` is only used for the initial window creation.
- **PTY slave lifetime**: drop `pair.slave` after `spawn_command` — keeping it open can cause the reader thread to never see EOF when zsh exits.
