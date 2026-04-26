use std::io::{self, BufRead, Write};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;

// ── Catppuccin Mocha palette ──────────────────────────────────────────────────

const LAVENDER: (u8, u8, u8) = (0xb4, 0xbe, 0xfe); // file header names
const MAUVE: (u8, u8, u8) = (0xcb, 0xa6, 0xf7); // hunk header @@
const GREEN: (u8, u8, u8) = (0xa6, 0xe3, 0xa1); // added prefix +
const RED: (u8, u8, u8) = (0xf3, 0x8b, 0xa8); // removed prefix -
const OVERLAY0: (u8, u8, u8) = (0x6c, 0x70, 0x86); // meta / index lines
const SURFACE1: (u8, u8, u8) = (0x45, 0x47, 0x5a); // subtle separators

// Background tints for diff lines (dark, not too loud)
const BG_ADDED: (u8, u8, u8) = (23, 51, 34);
const BG_REMOVED: (u8, u8, u8) = (58, 20, 28);
const BG_HUNK: (u8, u8, u8) = (28, 25, 44);

// ── ANSI helpers ──────────────────────────────────────────────────────────────

fn fg(out: &mut impl Write, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(out, "\x1b[38;2;{r};{g};{b}m")
}

fn bg(out: &mut impl Write, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(out, "\x1b[48;2;{r};{g};{b}m")
}

fn reset(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[0m")
}

fn bold(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[1m")
}

/// Strip bare ANSI escape sequences from a string so we can process raw content.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some(']') => {
                    // OSC sequence — terminated by BEL (0x07) or ST (ESC \).
                    chars.next(); // consume ']'
                    while let Some(nc) = chars.next() {
                        if nc == '\x07' {
                            break;
                        }
                        if nc == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    // CSI or other ESC sequence — consume until alphabetic final byte.
                    for nc in chars.by_ref() {
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── Syntect helpers ───────────────────────────────────────────────────────────

fn make_highlighter<'a>(
    syntax_name: &str,
    ps: &'a SyntaxSet,
    ts: &'a ThemeSet,
) -> HighlightLines<'a> {
    let syntax = ps
        .find_syntax_by_name(syntax_name)
        .or_else(|| ps.find_syntax_by_extension(syntax_name))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let theme = ["base16-mocha.dark", "base16-ocean.dark", "Solarized (dark)"]
        .iter()
        .find_map(|n| ts.themes.get(*n))
        .or_else(|| ts.themes.values().next())
        .expect("no themes");
    HighlightLines::new(syntax, theme)
}

/// Extract the file path from a `--- a/path` or `+++ b/path` header line.
fn parse_file_path(header: &str) -> &str {
    // Strip leading "a/" or "b/" that git adds
    let p = header.trim();
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    // Strip /dev/null (binary / new-file cases)
    if p.starts_with("/dev/null") { "" } else { p }
}

/// Infer a syntect extension/name hint from a file path.
fn syntax_hint(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

// ── Per-line highlighter ──────────────────────────────────────────────────────

/// Highlights a code line using a running HighlightLines (state advances per call).
fn hl_line<'s>(line: &'s str, h: &mut HighlightLines, ps: &SyntaxSet) -> Vec<(Style, String)> {
    let with_nl = if line.ends_with('\n') {
        std::borrow::Cow::Borrowed(line)
    } else {
        std::borrow::Cow::Owned(format!("{line}\n"))
    };
    h.highlight_line(with_nl.as_ref(), ps)
        .unwrap_or_default()
        .into_iter()
        .map(|(s, t)| (s, t.trim_end_matches('\n').to_string()))
        .collect()
}

fn write_code_line(
    out: &mut impl Write,
    prefix_char: &str,
    prefix_color: (u8, u8, u8),
    spans: &[(Style, String)],
    line_bg: (u8, u8, u8),
) -> io::Result<()> {
    bg(out, line_bg.0, line_bg.1, line_bg.2)?;
    // Colored prefix sigil
    bold(out)?;
    fg(out, prefix_color.0, prefix_color.1, prefix_color.2)?;
    write!(out, "{prefix_char}")?;
    // Syntax-colored content
    for (style, text) in spans {
        if text.is_empty() {
            continue;
        }
        let s = style.foreground;
        fg(out, s.r, s.g, s.b)?;
        if style
            .font_style
            .contains(syntect::highlighting::FontStyle::BOLD)
        {
            out.write_all(b"\x1b[1m")?;
        }
        if style
            .font_style
            .contains(syntect::highlighting::FontStyle::ITALIC)
        {
            out.write_all(b"\x1b[3m")?;
        }
        out.write_all(text.as_bytes())?;
    }
    out.write_all(b"\x1b[K")?; // fill rest of line with bg tint
    reset(out)?;
    writeln!(out)
}

// ── Main rendering loop ───────────────────────────────────────────────────────

fn run(out: &mut impl Write) -> io::Result<()> {
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

    let stdin = io::stdin();
    let reader = stdin.lock();

    let mut hint = String::new(); // current file extension/name
    let mut new_hl: Option<HighlightLines> = None;
    let mut old_hl: Option<HighlightLines> = None;

    let (lr, lg, lb) = LAVENDER;
    let (mr, mg, mb) = MAUVE;
    let (sfr, sfg, sfb) = SURFACE1;
    let (or_, og, ob) = OVERLAY0;

    for raw_line in reader.lines() {
        let raw = raw_line?;
        // Strip ANSI in case git was called with --color=always
        let line = strip_ansi(&raw);

        if line.starts_with("diff ") {
            // New file diff — print separator
            reset(out)?;
            fg(out, sfr, sfg, sfb)?;
            writeln!(
                out,
                "──────────────────────────────────────────────────────"
            )?;
            reset(out)?;
            fg(out, or_, og, ob)?;
            writeln!(out, "{line}")?;
            reset(out)?;
            new_hl = None;
            old_hl = None;
            hint.clear();
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let path = parse_file_path(rest);
            if !path.is_empty() {
                let ext = syntax_hint(path);
                if !ext.is_empty() {
                    hint = ext.to_string();
                }
            }
            fg(out, lr, lg, lb)?;
            bold(out)?;
            write!(out, "--- ")?;
            reset(out)?;
            fg(out, lr, lg, lb)?;
            writeln!(out, "{}", parse_file_path(rest))?;
            reset(out)?;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = parse_file_path(rest);
            // (Re)initialize highlighters for this file
            new_hl = Some(make_highlighter(&hint, &ps, &ts));
            old_hl = Some(make_highlighter(&hint, &ps, &ts));

            fg(out, lr, lg, lb)?;
            bold(out)?;
            write!(out, "+++ ")?;
            reset(out)?;
            fg(out, lr, lg, lb)?;
            writeln!(out, "{path}")?;
            reset(out)?;
        } else if line.starts_with("@@ ") {
            // Parse @@ -old +new @@ optional_tail
            // Reset per-hunk state
            new_hl = Some(make_highlighter(&hint, &ps, &ts));
            old_hl = Some(make_highlighter(&hint, &ps, &ts));

            // Style: mauve @@ numbers @@, dim tail
            let (at_end, tail) = if let Some(idx) = line[2..].find("@@") {
                let end = 2 + idx + 2;
                (&line[..end], line[end..].trim())
            } else {
                (line.as_str(), "")
            };

            bg(out, BG_HUNK.0, BG_HUNK.1, BG_HUNK.2)?;
            bold(out)?;
            fg(out, mr, mg, mb)?;
            write!(out, "{at_end}")?;
            if !tail.is_empty() {
                reset(out)?;
                bg(out, BG_HUNK.0, BG_HUNK.1, BG_HUNK.2)?;
                fg(out, or_, og, ob)?;
                write!(out, " {tail}")?;
            }
            out.write_all(b"\x1b[K")?;
            reset(out)?;
            writeln!(out)?;
        } else if let Some(content) = line.strip_prefix('+') {
            let hl = new_hl.get_or_insert_with(|| make_highlighter(&hint, &ps, &ts));
            let spans = hl_line(content, hl, &ps);
            // Also advance old_hl with a context-like phantom to keep state roughly in sync
            if let Some(old) = old_hl.as_mut() {
                let _ = hl_line(content, old, &ps);
            }
            write_code_line(out, "+", GREEN, &spans, BG_ADDED)?;
        } else if let Some(content) = line.strip_prefix('-') {
            let hl = old_hl.get_or_insert_with(|| make_highlighter(&hint, &ps, &ts));
            let spans = hl_line(content, hl, &ps);
            write_code_line(out, "-", RED, &spans, BG_REMOVED)?;
        } else if let Some(content) = line.strip_prefix(' ') {
            // Context line — advance both highlighters
            let spans = if let Some(hl) = new_hl.as_mut() {
                let s = hl_line(content, hl, &ps);
                if let Some(old) = old_hl.as_mut() {
                    let _ = hl_line(content, old, &ps);
                }
                s
            } else {
                vec![(Style::default(), content.to_string())]
            };
            write_code_line(out, " ", OVERLAY0, &spans, (0x1e, 0x1e, 0x2e))?;
        } else if line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
        {
            fg(out, or_, og, ob)?;
            writeln!(out, "{line}")?;
            reset(out)?;
        } else if line.starts_with('\\') {
            // \ No newline at end of file
            fg(out, or_, og, ob)?;
            writeln!(out, "{line}")?;
            reset(out)?;
        } else {
            // Unrecognised line — pass through
            writeln!(out, "{line}")?;
        }
    }

    reset(out)?;
    out.flush()
}

fn main() {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if let Err(e) = run(&mut out) {
        // Broken pipe (e.g. user quits less) is normal
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("tdiff: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_ansi ────────────────────────────────────────────────────────────

    #[test]
    fn test_strip_ansi_plain_text_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn test_strip_ansi_removes_csi_color_sequence() {
        // \x1b[31m — CSI 'm' is the final alphabetic byte.
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn test_strip_ansi_removes_multiple_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[32mbold green\x1b[0m"), "bold green");
    }

    #[test]
    fn test_strip_ansi_osc_bel_terminated_fully_stripped() {
        // OSC sequence \x1b]0;Title\x07 — title is terminated by BEL (0x07),
        // not by an alphabetic byte. The broken implementation stops at 'T'
        // and leaks "itle\x07". The fix handles OSC specially.
        assert_eq!(
            strip_ansi("Hello\x1b]0;Title\x07World"),
            "HelloWorld",
            "OSC BEL-terminated sequence must be fully consumed"
        );
    }

    #[test]
    fn test_strip_ansi_osc_st_terminated_fully_stripped() {
        // OSC sequence terminated by ST (ESC \).
        assert_eq!(
            strip_ansi("\x1b]2;My Window\x1b\\done"),
            "done",
            "OSC ST-terminated sequence must be fully consumed"
        );
    }

    #[test]
    fn test_strip_ansi_lone_osc_no_panic() {
        // OSC without a terminator — must not loop forever or panic.
        let result = strip_ansi("\x1b]unterminated");
        assert_eq!(result, "", "unterminated OSC must produce empty output");
    }

    // ── parse_file_path ───────────────────────────────────────────────────────

    #[test]
    fn test_parse_file_path_strips_a_prefix() {
        assert_eq!(parse_file_path("a/src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_parse_file_path_strips_b_prefix() {
        assert_eq!(parse_file_path("b/src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_parse_file_path_no_prefix_unchanged() {
        assert_eq!(parse_file_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_parse_file_path_dev_null_empty() {
        assert_eq!(parse_file_path("/dev/null"), "");
    }

    #[test]
    fn test_parse_file_path_dev_null_with_annotation_empty() {
        // git appends "(new file)" or "(deleted)" after /dev/null in some
        // diff formats. Must still resolve to empty.
        assert_eq!(
            parse_file_path("/dev/null (new file)"),
            "",
            "/dev/null with annotation must resolve to empty path"
        );
    }

    #[test]
    fn test_parse_file_path_whitespace_trimmed() {
        assert_eq!(parse_file_path("  a/foo.rs  "), "foo.rs");
    }

    // ── syntax_hint ───────────────────────────────────────────────────────────

    #[test]
    fn test_syntax_hint_rust_extension() {
        assert_eq!(syntax_hint("src/main.rs"), "rs");
    }

    #[test]
    fn test_syntax_hint_python_extension() {
        assert_eq!(syntax_hint("script.py"), "py");
    }

    #[test]
    fn test_syntax_hint_no_extension_returns_empty() {
        assert_eq!(syntax_hint("Makefile"), "");
    }

    #[test]
    fn test_syntax_hint_dotfile_no_ext_returns_empty() {
        // ".gitignore" has no extension (the whole name is the stem).
        assert_eq!(syntax_hint(".gitignore"), "");
    }

    #[test]
    fn test_syntax_hint_empty_path_returns_empty() {
        assert_eq!(syntax_hint(""), "");
    }

    #[test]
    fn test_syntax_hint_uses_last_component() {
        assert_eq!(syntax_hint("a/b/c/foo.ts"), "ts");
    }

    // ── write_code_line ───────────────────────────────────────────────────────

    #[test]
    fn test_write_code_line_contains_prefix_char() {
        // write_code_line must emit the prefix sigil somewhere in its output.
        let mut out = Vec::<u8>::new();
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let spans = hl_line("let x = 1;", &mut make_highlighter("rs", &ps, &ts), &ps);
        write_code_line(&mut out, "+", (0, 255, 0), &spans, (30, 40, 30)).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains('+'), "output must contain the prefix sigil '+'");
    }

    #[test]
    fn test_write_code_line_ends_with_newline() {
        let mut out = Vec::<u8>::new();
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let spans = hl_line("x", &mut make_highlighter("rs", &ps, &ts), &ps);
        write_code_line(&mut out, "-", (255, 0, 0), &spans, (40, 20, 20)).unwrap();
        assert!(
            out.ends_with(b"\n"),
            "write_code_line must end with newline"
        );
    }

    #[test]
    fn test_write_code_line_empty_spans_still_writes_prefix_and_newline() {
        let mut out = Vec::<u8>::new();
        write_code_line(&mut out, " ", (200, 200, 200), &[], (0, 0, 0)).unwrap();
        let s = String::from_utf8_lossy(&out);
        // Even with no spans, prefix and newline must be present.
        assert!(out.ends_with(b"\n"));
        // Must contain some ANSI sequences (bg, bold, reset).
        assert!(s.contains('\x1b'), "must emit at least one ANSI escape");
    }

    // ── strip_ansi edge cases ─────────────────────────────────────────────────

    #[test]
    fn test_strip_ansi_lone_esc_not_followed_by_bracket_consumed() {
        // A bare ESC that is NOT followed by '[' or ']' — the current
        // implementation falls through to the CSI branch which consumes until
        // the next alphabetic byte.  Either way the result must not contain
        // the ESC byte.
        let result = strip_ansi("\x1b7hello");
        assert!(
            !result.contains('\x1b'),
            "lone ESC must not appear in output; got: {result:?}"
        );
    }

    #[test]
    fn test_strip_ansi_multiple_osc_sequences_all_stripped() {
        let result = strip_ansi("\x1b]0;first\x07\x1b]2;second\x07done");
        assert_eq!(result, "done");
    }

    #[test]
    fn test_strip_ansi_adjacent_csi_sequences() {
        // Two back-to-back CSI sequences — both must be stripped.
        let result = strip_ansi("\x1b[1m\x1b[0mtext");
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_preserves_unicode_text() {
        let result = strip_ansi("héllo\x1b[31m wörld\x1b[0m");
        assert_eq!(result, "héllo wörld");
    }
}
