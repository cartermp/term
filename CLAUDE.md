# term

Mac-native terminal emulator written in Rust. Runs zsh, renders via a CPU framebuffer backed by Metal (softbuffer), rasterizes glyphs with fontdue. Catppuccin Mocha color theme.

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
| `src/terminal.rs` | VT/ANSI state machine (`vte::Perform`), terminal grid, `Terminal` wrapper |
| `src/renderer.rs` | fontdue glyph cache, per-frame blit to `u32` framebuffer |
| `src/main.rs` | winit 0.30 event loop, softbuffer surface, PTY lifecycle, keyboard input |

### Data flow

```
zsh (PTY slave)
  └─ PTY master → reader thread
                      └─ EventLoopProxy::send_event(AppEvent::PtyData)
                                └─ terminal.process(bytes)   ← vte parser
                                       └─ window.request_redraw()
                                              └─ renderer.render() → softbuffer
```

Keyboard input goes the other direction: `winit KeyboardInput → handle_key → pty_writer`.

### Terminal emulation

`TerminalState` owns the grid (`Vec<Vec<Cell>>`) and implements `vte::Perform`. Supported:
- SGR colors: standard ANSI 8/16, 256-color (`38;5;n`), true-color (`38;2;r;g;b`)
- Cursor movement: CUP, CUU/D/F/B, CHA, VPA, home/end
- Erase: ED (0/1/2/3), EL (0/1/2), ECH
- Scroll region: DECSTBM (`r`), scroll up/down (SU/SD, IL/DL)
- Insert/delete chars: ICH (`@`), DCH (`P`)
- Cursor save/restore: ESC 7/8 and CSI s/u
- Device status report: `ESC[6n` → cursor position reply
- OSC 0/2: window title
- Private modes (`?h`/`?l`): accepted but mostly no-op

### Rendering

`Renderer::render` is called every frame:
1. Fill framebuffer with `DEFAULT_BG`
2. For each visible cell: fill cell background, blit cached glyph with alpha compositing
3. Cursor drawn as a solid block (CURSOR_COLOR bg, DEFAULT_BG fg) at the cursor cell

Glyph cache key is `(char, bold)`. Cache is never evicted (terminals use a small glyph set).

## Syntax highlighting (`tcat` and `tdiff`)

`src/bin/tcat.rs` reads a file, applies 24-bit true-color ANSI syntax highlighting via `syntect` (theme: `base16-ocean.dark`), and prints to stdout.

`src/bin/tdiff.rs` reads a unified diff from stdin, applies syntax highlighting to the code content, and renders added/removed lines with green/red background tints (Catppuccin Mocha palette). It is used as `GIT_PAGER` so `git diff`, `git show`, `git log -p`, etc. automatically render with syntax colors.

On startup, `term` writes a ZDOTDIR-based zsh init that:
1. Sources the user's real `~/.zshenv` and `~/.zshrc`
2. Defines `function cat()` that calls `tcat` for single-file invocations
3. Sets `GIT_PAGER=tdiff` and `GIT_COLOR_UI=never` so git hands raw diff to `tdiff`

Both `tcat` and `tdiff` live next to the `term` binary in the build output dir. `cargo build` builds all three.

## Cursor

A narrow 2-physical-pixel vertical bar drawn on top of the current cell after the full cell pass. Color: `CURSOR_COLOR`. Blinks at ~530 ms on/off via `about_to_wait` + `ControlFlow::WaitUntil`. Blink resets (cursor always shown) on keypress or PTY output.

## Crate versions (pinned in Cargo.lock)

- `winit 0.30` — windowing, `ApplicationHandler` API
- `softbuffer 0.4` — Metal-backed CPU framebuffer on macOS
- `vte 0.13` — VT/ANSI parser
- `portable-pty 0.8` — PTY open + zsh spawn
- `fontdue 0.8` — pure-Rust glyph rasterizer
- `syntect 5` — syntax highlighting for `tcat` (features: `default-syntaxes`, `default-themes`, `parsing`, `regex-fancy`)

## Font

Loads the first available path at startup:
1. `/System/Library/Fonts/Menlo.ttc` (default macOS)
2. `/System/Library/Fonts/Monaco.ttf`
3. `/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf`

Panics at startup if none found. To change the font size, edit `FONT_SIZE_PT` in `src/config.rs`.

## Known gaps / future work

- No scrollback view (lines scroll off and are gone)
- No clipboard (Cmd+V is a no-op)
- No mouse support
- No ligatures or double-width characters
- No alternate screen buffer (vim/htop work via scroll region but no true alt screen)
- Bold uses same font (no separate bold face loaded)
