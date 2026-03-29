# Features

## Terminal emulator (`term`)

- GPU-accelerated Metal rendering via wgpu (rect + glyph instanced pipelines)
- JetBrains Mono bundled font; no system font dependency
- Catppuccin Mocha color theme
- True-color ANSI (8/16, 256-color, 24-bit RGB)
- 10,000-line scrollback buffer (mouse wheel, Cmd+Up/Down/Home/End)
- Multiple tabs (Cmd+T/W, Cmd+[/], Cmd+1–9, drag to reorder)
- Dynamic tab titles (CWD at prompt, command name while running)
- Clipboard: Cmd+C copy (text), Cmd+V paste (text or image path), OSC 52
- URL detection with Cmd+underline and Cmd+click to open
- Inline history ghost text from ~/.zsh_history (accept with Cmd+Right)
- Alternate screen buffer (?1049h) — vim, htop, etc.
- Block/Braille character rendering (U+2580–U+259F, U+2800–U+28FF) as fill rects
- Bracketed paste mode (?2004h)
- Shell integration: ZLE buffer/cursor via OSC 9001, working directory via OSC 7
- Mouse: scroll wheel, click-drag text selection, auto-scroll during drag
- Blinking 2px cursor bar (~530 ms)
- LANG/LC_ALL=en_US.UTF-8 injected at startup (prevents Mac Roman garbling)

## VT/ANSI sequences

- SGR: bold, italic, underline, inverse, all color modes
- Cursor: CUP, CUU/D/F/B, CHA, VPA, home/end, save/restore (ESC 7/8 and CSI s/u)
- Erase: ED (0/1/2/3), EL (0/1/2), ECH
- Scroll region: DECSTBM, SU, SD
- Insert/delete: ICH, DCH, IL, DL
- Device attributes and status report
- OSC 0/2 (title), OSC 7 (cwd), OSC 52 (clipboard), OSC 9001 (shell integration)
- Private modes: ?47, ?1047, ?1049 (alt screen), ?2004 (bracketed paste)

## `tcat` — syntax-highlighted file viewer

- Syntax highlighting via syntect (base16-ocean.dark theme)
- Line numbers in gutter
- File info header (name, language, directory)
- Line range support: `tcat file.rs:40-70`, `tcat file.rs:42`
- Automatic via `cat` alias for single-file invocations

## `tdiff` — syntax-highlighted diff pager

- Processes unified diff format (stdin)
- Syntax highlights added/removed line content
- Green/red background tints per line type
- Colored hunk headers (@@) in Catppuccin Mauve
- Set as GIT_PAGER automatically (git diff, git show, git log -p)

## `tjson` — streaming JSON prettifier

- Filter mode: `cmd | json` — reads stdin line by line
- PTY mode: `json cmd [args]` — spawns command in PTY (child sees real terminal)
- JSON lines (starting with `{` or `[`) are pretty-printed with syntax highlighting
- Non-JSON lines pass through unchanged
- Exit code propagated in PTY mode
- nice drag ui
