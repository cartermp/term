use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthChar;

// ── ANSI helpers ──────────────────────────────────────────────────────────────

/// Write a true-colour foreground sequence.
fn fg(out: &mut impl Write, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(out, "\x1b[38;2;{r};{g};{b}m")
}
/// Reset all attributes.
fn reset(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[0m")
}
/// Bold on.
fn bold(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[1m")
}

// Catppuccin Mocha palette used for chrome (not syntax — that comes from the theme).
const LAVENDER: (u8, u8, u8) = (0xb4, 0xbe, 0xfe); // identifiers / header name
const SUBTEXT0: (u8, u8, u8) = (0xa6, 0xad, 0xc8); // language badge
const OVERLAY0: (u8, u8, u8) = (0x6c, 0x70, 0x86); // line numbers + gutter
const SURFACE1: (u8, u8, u8) = (0x45, 0x47, 0x5a); // subtle separator
const GREEN: (u8, u8, u8) = (0xa6, 0xe3, 0xa1); // language highlight

fn gutter_prefix_width(gutter_width: usize) -> usize {
    gutter_width + 5
}

fn configured_columns() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&cols| cols > 0)
        .or_else(stdout_columns)
}

fn stdout_columns() -> Option<usize> {
    let stdout = io::stdout();
    if !stdout.is_terminal() {
        return None;
    }

    let fd = stdout.as_raw_fd();
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` comes from stdout, `ws` is a valid mutable winsize buffer,
    // and TIOCGWINSZ only writes to that buffer.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

fn content_wrap_width(gutter_width: usize) -> Option<usize> {
    configured_columns().map(|cols| {
        cols.saturating_sub(gutter_prefix_width(gutter_width))
            .max(1)
    })
}

fn strip_line_endings(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

fn fitting_prefix_len(text: &str, available_width: usize, at_line_start: bool) -> usize {
    if text.is_empty() {
        return 0;
    }

    let mut width = 0usize;
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > available_width {
            if idx == 0 && at_line_start {
                return ch.len_utf8();
            }
            return end;
        }
        width += ch_width;
        end = idx + ch.len_utf8();
    }
    end
}

fn print_gutter(
    out: &mut impl Write,
    lineno: Option<usize>,
    gutter_width: usize,
) -> io::Result<()> {
    let (or_, og, ob) = OVERLAY0;
    let (sfr, sfg, sfb) = SURFACE1;

    out.write_all(b"  ")?;
    fg(out, or_, og, ob)?;
    match lineno {
        Some(n) => write!(out, "{:>width$}", n, width = gutter_width)?,
        None => write!(out, "{:>width$}", "", width = gutter_width)?,
    }
    out.write_all(b" ")?;
    fg(out, sfr, sfg, sfb)?;
    out.write_all("│".as_bytes())?;
    fg(out, or_, og, ob)?;
    out.write_all(b" ")?;
    reset(out)
}

fn write_highlighted_line(
    out: &mut impl Write,
    lineno: usize,
    gutter_width: usize,
    wrap_width: Option<usize>,
    ranges: &[(Style, &str)],
) -> io::Result<()> {
    print_gutter(out, Some(lineno), gutter_width)?;

    let mut used_width = 0usize;
    for (style, text) in ranges {
        let mut rest = strip_line_endings(text);
        while !rest.is_empty() {
            if let Some(limit) = wrap_width {
                let available = limit.saturating_sub(used_width);
                let split_at = fitting_prefix_len(rest, available, used_width == 0);
                if split_at == 0 {
                    writeln!(out)?;
                    print_gutter(out, None, gutter_width)?;
                    used_width = 0;
                    continue;
                }

                let chunk = &rest[..split_at];
                write_span(out, *style, chunk)?;
                used_width += chunk
                    .chars()
                    .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                    .sum::<usize>();
                rest = &rest[split_at..];
                if !rest.is_empty() {
                    writeln!(out)?;
                    print_gutter(out, None, gutter_width)?;
                    used_width = 0;
                }
            } else {
                write_span(out, *style, rest)?;
                rest = "";
            }
        }
    }

    writeln!(out)
}

// ── Render a single highlighted span ─────────────────────────────────────────

fn write_span(out: &mut impl Write, style: Style, text: &str) -> io::Result<()> {
    let s = style.foreground;
    // Skip spans that are pure-background or invisible
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
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        out.write_all(b"\x1b[4m")?;
    }
    out.write_all(text.as_bytes())?;
    reset(out)
}

// ── Path / range parsing ──────────────────────────────────────────────────────

/// Split `"path/file.rs:40-70"` into `("path/file.rs", Some((40, 70)))`.
/// Also accepts `"file.rs:40"` as a single-line range `Some((40, 40))`.
/// Returns `(arg, None)` unchanged if no valid range suffix is found.
fn parse_path_range(arg: &str) -> (&str, Option<(usize, usize)>) {
    if let Some(colon) = arg.rfind(':') {
        let range_str = &arg[colon + 1..];
        let path = &arg[..colon];
        if let Some(dash) = range_str.find('-') {
            let (s, e) = (&range_str[..dash], &range_str[dash + 1..]);
            if let (Ok(start), Ok(end)) = (s.parse::<usize>(), e.parse::<usize>()) {
                return (path, Some((start, end)));
            }
        }
        if let Ok(line) = range_str.parse::<usize>() {
            return (path, Some((line, line)));
        }
    }
    (arg, None)
}

// ── Header ────────────────────────────────────────────────────────────────────

fn print_header(
    out: &mut impl Write,
    path: &str,
    lang: &str,
    range: Option<(usize, usize)>,
) -> io::Result<()> {
    // Extract just the filename for display
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or(std::borrow::Cow::Borrowed(path));

    // ╭─ filename.rs ──── Rust ─╮  (we skip the box, just use a clean bar)
    //  ╭─ filename  [Language]
    let (lr, lg, lb) = LAVENDER;
    let (sr, sg, sb) = SUBTEXT0;
    let (gr, gg, gb) = GREEN;
    let (or_, og, ob) = OVERLAY0;
    let (sfr, sfg, sfb) = SURFACE1;

    // Top rule
    fg(out, sfr, sfg, sfb)?;
    out.write_all("  ╭─".as_bytes())?;
    reset(out)?;
    out.write_all(b" ")?;

    // Filename
    bold(out)?;
    fg(out, lr, lg, lb)?;
    write!(out, "{name}")?;
    reset(out)?;

    // Line range suffix, e.g. ":40–70"
    if let Some((start, end)) = range {
        fg(out, or_, og, ob)?;
        if start == end {
            write!(out, ":{start}")?;
        } else {
            write!(out, ":{start}–{end}")?;
        }
        reset(out)?;
    }

    if !lang.is_empty() && lang != "Plain Text" {
        // separator dots
        fg(out, or_, og, ob)?;
        out.write_all("  ·  ".as_bytes())?;
        reset(out)?;

        fg(out, gr, gg, gb)?;
        bold(out)?;
        write!(out, "{lang}")?;
        reset(out)?;
    }

    // show directory path in dim
    let dir = std::path::Path::new(path)
        .parent()
        .and_then(|p| {
            if p.as_os_str().is_empty() {
                None
            } else {
                Some(p)
            }
        })
        .map(|p| p.to_string_lossy().to_string());
    if let Some(d) = dir {
        out.write_all(b"  ")?;
        fg(out, sr, sg, sb)?;
        write!(out, "{d}/")?;
        reset(out)?;
    }

    writeln!(out)?;

    // Thin rule below header
    fg(out, sfr, sfg, sfb)?;
    out.write_all("  │".as_bytes())?;
    reset(out)?;
    writeln!(out)?;

    Ok(())
}

fn print_footer(out: &mut impl Write) -> io::Result<()> {
    let (sfr, sfg, sfb) = SURFACE1;
    fg(out, sfr, sfg, sfb)?;
    out.write_all("  ╰─".as_bytes())?;
    reset(out)?;
    writeln!(out)?;
    Ok(())
}

// ── Core highlight ────────────────────────────────────────────────────────────

fn highlight_file(
    path: &str,
    range: Option<(usize, usize)>,
    ps: &SyntaxSet,
    ts: &ThemeSet,
) -> io::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;

    let syntax = ps
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| ps.find_syntax_plain_text());

    let lang = syntax.name.as_str();

    // Prefer a warm dark theme that complements Catppuccin Mocha.
    // Fallback chain: Mocha → ocean → first dark theme available.
    let theme = ["base16-mocha.dark", "base16-ocean.dark", "Solarized (dark)"]
        .iter()
        .find_map(|n| ts.themes.get(*n))
        .or_else(|| ts.themes.values().next())
        .expect("syntect has no themes");

    let mut h = HighlightLines::new(syntax, theme);
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    print_header(&mut out, path, lang, range)?;

    let total_lines = LinesWithEndings::from(&content).count();
    // Gutter width based on the last visible line number.
    let last_visible = range
        .map(|(_, e)| e.min(total_lines))
        .unwrap_or(total_lines);
    let gutter_width = last_visible.to_string().len().max(2);
    let wrap_width = content_wrap_width(gutter_width);

    for (i, line) in LinesWithEndings::from(&content).enumerate() {
        let lineno = i + 1;

        if let Some((start, end)) = range {
            if lineno < start {
                // Still must feed lines to the highlighter to maintain parser state.
                let _ = h.highlight_line(line, ps);
                continue;
            }
            if lineno > end {
                break;
            }
        }

        let ranges = h.highlight_line(line, ps).unwrap_or_default();
        write_highlighted_line(&mut out, lineno, gutter_width, wrap_width, &ranges)?;
    }

    print_footer(&mut out)?;
    out.flush()
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        // stdin passthrough
        let mut buf = Vec::new();
        if let Err(e) = io::stdin().read_to_end(&mut buf) {
            eprintln!("tcat: stdin: {e}");
            std::process::exit(1);
        }
        if let Err(e) = io::stdout().write_all(&buf) {
            eprintln!("tcat: stdout: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Any flag (e.g. -n, -A) → fall through to /bin/cat
    if args.iter().any(|a| a.starts_with('-')) {
        let status = std::process::Command::new("/bin/cat").args(&args).status();
        let code = status.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
        std::process::exit(code);
    }

    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

    let mut exit_code = 0i32;
    for arg in &args {
        let (path, range) = parse_path_range(arg);
        if let Err(e) = highlight_file(path, range, &ps, &ts) {
            eprintln!("tcat: {e}");
            exit_code = 1;
        }
    }
    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntect::highlighting::{Color, FontStyle, Style};

    fn color(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    fn plain_style(r: u8, g: u8, b: u8) -> Style {
        Style {
            foreground: color(r, g, b),
            background: color(0, 0, 0),
            font_style: FontStyle::empty(),
        }
    }

    fn capture(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.peek().copied() {
                    Some(']') => {
                        chars.next();
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

    // ── ANSI helpers ──────────────────────────────────────────────────────────

    #[test]
    fn test_fg_sequence() {
        let out = capture(|b| fg(b, 255, 128, 0));
        assert_eq!(out, "\x1b[38;2;255;128;0m");
    }

    #[test]
    fn test_reset_sequence() {
        let out = capture(|b| reset(b));
        assert_eq!(out, "\x1b[0m");
    }

    #[test]
    fn test_bold_sequence() {
        let out = capture(|b| bold(b));
        assert_eq!(out, "\x1b[1m");
    }

    // ── write_span ────────────────────────────────────────────────────────────

    #[test]
    fn test_write_span_plain() {
        let style = plain_style(100, 200, 50);
        let out = capture(|b| write_span(b, style, "hello"));
        // fg + text + reset
        assert!(
            out.starts_with("\x1b[38;2;100;200;50m"),
            "missing fg: {out:?}"
        );
        assert!(out.contains("hello"), "missing text: {out:?}");
        assert!(out.ends_with("\x1b[0m"), "missing reset: {out:?}");
    }

    #[test]
    fn test_write_span_bold() {
        let style = Style {
            foreground: color(1, 2, 3),
            background: color(0, 0, 0),
            font_style: FontStyle::BOLD,
        };
        let out = capture(|b| write_span(b, style, "x"));
        assert!(out.contains("\x1b[1m"), "missing bold: {out:?}");
    }

    #[test]
    fn test_write_span_italic() {
        let style = Style {
            foreground: color(1, 2, 3),
            background: color(0, 0, 0),
            font_style: FontStyle::ITALIC,
        };
        let out = capture(|b| write_span(b, style, "x"));
        assert!(out.contains("\x1b[3m"), "missing italic: {out:?}");
    }

    #[test]
    fn test_write_span_underline() {
        let style = Style {
            foreground: color(1, 2, 3),
            background: color(0, 0, 0),
            font_style: FontStyle::UNDERLINE,
        };
        let out = capture(|b| write_span(b, style, "x"));
        assert!(out.contains("\x1b[4m"), "missing underline: {out:?}");
    }

    #[test]
    fn test_write_span_empty_text() {
        // empty span should still emit fg + reset (caller skips via the !t.is_empty() guard,
        // but write_span itself must not panic)
        let style = plain_style(10, 20, 30);
        capture(|b| write_span(b, style, ""));
    }

    // ── Header / footer ───────────────────────────────────────────────────────

    #[test]
    fn test_print_header_contains_filename() {
        let out = capture(|b| print_header(b, "src/main.rs", "Rust", None));
        assert!(out.contains("main.rs"), "filename not found: {out:?}");
    }

    #[test]
    fn test_print_header_contains_lang() {
        let out = capture(|b| print_header(b, "foo.rs", "Rust", None));
        assert!(out.contains("Rust"), "lang not found: {out:?}");
    }

    #[test]
    fn test_print_header_plain_text_lang_hidden() {
        let out = capture(|b| print_header(b, "notes.txt", "Plain Text", None));
        assert!(
            !out.contains("Plain Text"),
            "plain text lang leaked: {out:?}"
        );
    }

    #[test]
    fn test_print_header_empty_lang_hidden() {
        let out = capture(|b| print_header(b, "notes", "", None));
        assert!(
            !out.contains("·"),
            "separator shown for empty lang: {out:?}"
        );
    }

    #[test]
    fn test_print_header_with_directory() {
        let out = capture(|b| print_header(b, "src/foo.rs", "Rust", None));
        assert!(out.contains("src/"), "directory not shown: {out:?}");
    }

    #[test]
    fn test_print_header_no_directory_for_bare_filename() {
        let out = capture(|b| print_header(b, "foo.rs", "Rust", None));
        assert!(!out.contains("//"), "double slash: {out:?}");
    }

    #[test]
    fn test_print_header_range_shown() {
        let out = capture(|b| print_header(b, "foo.rs", "Rust", Some((10, 30))));
        assert!(out.contains("10"), "start line missing: {out:?}");
        assert!(out.contains("30"), "end line missing: {out:?}");
    }

    #[test]
    fn test_print_header_single_line_range() {
        let out = capture(|b| print_header(b, "foo.rs", "Rust", Some((42, 42))));
        assert!(out.contains(":42"), "single line ref missing: {out:?}");
    }

    // ── parse_path_range ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_range_start_end() {
        assert_eq!(parse_path_range("foo.rs:10-50"), ("foo.rs", Some((10, 50))));
    }

    #[test]
    fn test_parse_range_single_line() {
        assert_eq!(
            parse_path_range("src/main.rs:42"),
            ("src/main.rs", Some((42, 42)))
        );
    }

    #[test]
    fn test_parse_range_none_for_plain_path() {
        assert_eq!(parse_path_range("src/main.rs"), ("src/main.rs", None));
    }

    #[test]
    fn test_parse_range_none_for_non_numeric_suffix() {
        // "foo.rs:bar" has a colon but no valid number — fall through unchanged
        assert_eq!(parse_path_range("foo.rs:bar"), ("foo.rs:bar", None));
    }

    #[test]
    fn test_parse_range_uses_last_colon() {
        // Path with multiple colons: only the last is treated as range separator
        assert_eq!(
            parse_path_range("a:b/foo.rs:5-10"),
            ("a:b/foo.rs", Some((5, 10)))
        );
    }

    // ── parse_path_range adversarial ──────────────────────────────────────────

    /// Simulate the highlight_file line-filter loop so we can test range
    /// behaviour without touching files or stdout.
    fn count_filtered_lines(total: usize, range: Option<(usize, usize)>) -> usize {
        let mut count = 0;
        for lineno in 1..=total {
            if let Some((start, end)) = range {
                if lineno < start {
                    continue;
                }
                if lineno > end {
                    break;
                }
            }
            count += 1;
        }
        count
    }

    #[test]
    fn test_reversed_range_produces_no_output() {
        // parse_path_range accepts reversed ranges without validation.
        // highlight_file skips lines below start (100), but end=50 means
        // the loop immediately breaks when lineno > 50 — zero lines shown.
        let (_, range) = parse_path_range("foo.rs:100-50");
        assert_eq!(
            range,
            Some((100, 50)),
            "reversed range must be parsed (not rejected at parse stage)"
        );
        assert_eq!(
            count_filtered_lines(200, range),
            0,
            "reversed range must produce zero visible lines"
        );
    }

    #[test]
    fn test_line_zero_produces_no_output() {
        // Line numbers are 1-indexed. parse_path_range accepts 0 without
        // validation; the filter then skips lines where lineno < 1 but
        // immediately breaks when lineno > 0 — zero lines shown.
        let (_, range) = parse_path_range("foo.rs:0");
        assert_eq!(
            range,
            Some((0, 0)),
            "line 0 must be parsed (not rejected at parse stage)"
        );
        assert_eq!(
            count_filtered_lines(100, range),
            0,
            "line 0 must produce zero visible lines (invalid 1-indexed line)"
        );
    }

    #[test]
    fn test_normal_range_filter_is_correct() {
        let (_, range) = parse_path_range("foo.rs:10-20");
        assert_eq!(count_filtered_lines(100, range), 11);
    }

    #[test]
    fn test_single_line_filter_is_correct() {
        let (_, range) = parse_path_range("foo.rs:42");
        assert_eq!(count_filtered_lines(100, range), 1);
    }

    #[test]
    fn test_print_footer_contains_corner() {
        let out = capture(|b| print_footer(b));
        assert!(out.contains("╰─"), "footer corner missing: {out:?}");
    }

    // ── Wrapping helpers ──────────────────────────────────────────────────────

    #[test]
    fn test_fitting_prefix_len_respects_visible_width() {
        assert_eq!(fitting_prefix_len("abcdef", 4, true), 4);
        assert_eq!(fitting_prefix_len("界ab", 2, true), "界".len());
        assert_eq!(fitting_prefix_len("界ab", 1, true), "界".len());
        assert_eq!(fitting_prefix_len("界ab", 1, false), 0);
    }

    #[test]
    fn test_write_highlighted_line_wraps_with_blank_continuation_gutter() {
        let style = plain_style(100, 200, 50);
        let ranges = vec![(style, "abcdefghij")];
        let out = capture(|b| write_highlighted_line(b, 12, 2, Some(6), &ranges));
        let clean = strip_ansi(&out);
        let lines: Vec<&str> = clean.lines().collect();
        assert_eq!(lines, vec!["  12 │ abcdef", "     │ ghij"]);
    }

    #[test]
    fn test_write_highlighted_line_preserves_next_real_line_number_after_wrap() {
        let style = plain_style(100, 200, 50);
        let first = capture(|b| write_highlighted_line(b, 8, 2, Some(5), &[(style, "abcdefgh")]));
        let second = capture(|b| write_highlighted_line(b, 9, 2, Some(5), &[(style, "xyz")]));
        let clean = format!("{}{}", strip_ansi(&first), strip_ansi(&second));
        let lines: Vec<&str> = clean.lines().collect();
        assert_eq!(lines, vec!["   8 │ abcde", "     │ fgh", "   9 │ xyz"]);
    }

    // ── CRLF handling ─────────────────────────────────────────────────────────

    #[test]
    fn test_crlf_stripping_leaves_no_cr() {
        assert_eq!(strip_line_endings("hello\r\n"), "hello");
    }

    #[test]
    fn test_lf_only_stripping() {
        assert_eq!(strip_line_endings("hello\n"), "hello");
    }

    #[test]
    fn test_no_newline_span_unchanged() {
        assert_eq!(strip_line_endings("world"), "world");
    }
}
