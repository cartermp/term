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
const PEACH: (u8, u8, u8) = (0xfa, 0xb3, 0x87); // commit hashes
const SKY: (u8, u8, u8) = (0x89, 0xdc, 0xeb); // author names
const YELLOW: (u8, u8, u8) = (0xf9, 0xe2, 0xaf); // ref decorations (HEAD, branches, tags)

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

// ── git log header recognition ────────────────────────────────────────────────

/// True iff the whole string is ASCII hex (lowercase or upper) and at least
/// 7 chars long — i.e. could plausibly be a git short or full SHA.
fn looks_like_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Try to render `line` as a `git log` header line (commit/Author/Date/Merge
/// /...) or a `--oneline` row. Returns `Ok(true)` when the line was handled.
fn try_render_log_line(line: &str, out: &mut impl Write) -> io::Result<bool> {
    // `commit <hash>` — optionally followed by ref decoration like
    //   `commit abc1234 (HEAD -> main, origin/main)`
    if let Some(rest) = line.strip_prefix("commit ") {
        let (hash, decoration) = match rest.find(' ') {
            Some(idx) => (&rest[..idx], Some(&rest[idx..])),
            None => (rest, None),
        };
        if looks_like_sha(hash) {
            fg(out, OVERLAY0.0, OVERLAY0.1, OVERLAY0.2)?;
            write!(out, "commit ")?;
            bold(out)?;
            fg(out, PEACH.0, PEACH.1, PEACH.2)?;
            write!(out, "{hash}")?;
            reset(out)?;
            if let Some(dec) = decoration {
                fg(out, YELLOW.0, YELLOW.1, YELLOW.2)?;
                write!(out, "{dec}")?;
                reset(out)?;
            }
            writeln!(out)?;
            return Ok(true);
        }
    }

    // `Author: Name <email>` — split name and email; dim the label and email,
    // colour the name.
    if let Some(rest) = line.strip_prefix("Author: ") {
        let (name, email) = match rest.rfind(" <") {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        fg(out, OVERLAY0.0, OVERLAY0.1, OVERLAY0.2)?;
        write!(out, "Author: ")?;
        fg(out, SKY.0, SKY.1, SKY.2)?;
        write!(out, "{name}")?;
        if !email.is_empty() {
            fg(out, OVERLAY0.0, OVERLAY0.1, OVERLAY0.2)?;
            write!(out, "{email}")?;
        }
        reset(out)?;
        writeln!(out)?;
        return Ok(true);
    }

    // Other plain-format header labels — all dim. Listed explicitly so we
    // don't accidentally swallow random `Foo: bar` lines.
    for label in [
        "Date:",
        "Merge:",
        "AuthorDate:",
        "CommitDate:",
        "Commit:",
        "Tag:",
        "Refs:",
    ] {
        if line.starts_with(label) {
            fg(out, OVERLAY0.0, OVERLAY0.1, OVERLAY0.2)?;
            writeln!(out, "{line}")?;
            reset(out)?;
            return Ok(true);
        }
    }

    // `--oneline` rows: `<hash> <subject>` (optionally with ` (refs)` between).
    if let Some(space) = line.find(' ') {
        let hash = &line[..space];
        let rest = &line[space + 1..];
        if looks_like_sha(hash) && !rest.is_empty() {
            bold(out)?;
            fg(out, PEACH.0, PEACH.1, PEACH.2)?;
            write!(out, "{hash}")?;
            reset(out)?;
            // Optional ref decoration before the subject.
            let (decoration, subject) = if rest.starts_with('(') {
                match rest.find(')') {
                    Some(close) => (Some(&rest[..=close]), rest[close + 1..].trim_start()),
                    None => (None, rest),
                }
            } else {
                (None, rest)
            };
            write!(out, " ")?;
            if let Some(dec) = decoration {
                fg(out, YELLOW.0, YELLOW.1, YELLOW.2)?;
                write!(out, "{dec} ")?;
                reset(out)?;
            }
            writeln!(out, "{subject}")?;
            return Ok(true);
        }
    }

    Ok(false)
}

// ── Main rendering loop ───────────────────────────────────────────────────────

fn render_lines<I>(lines: I, out: &mut impl Write) -> io::Result<()>
where
    I: IntoIterator<Item = io::Result<String>>,
{
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

    let mut hint = String::new(); // current file extension/name
    let mut new_hl: Option<HighlightLines> = None;
    let mut old_hl: Option<HighlightLines> = None;
    let mut in_diff = false;
    let mut in_hunk = false;

    let (lr, lg, lb) = LAVENDER;
    let (mr, mg, mb) = MAUVE;
    let (sfr, sfg, sfb) = SURFACE1;
    let (or_, og, ob) = OVERLAY0;

    for raw_line in lines {
        let raw = raw_line?;
        // Strip ANSI in case git was called with --color=always
        let line = strip_ansi(&raw);

        if line.starts_with("diff ") {
            in_diff = true;
            in_hunk = false;
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
        } else if in_diff && line.starts_with("index ") {
            fg(out, or_, og, ob)?;
            writeln!(out, "{line}")?;
            reset(out)?;
        } else if in_diff && let Some(rest) = line.strip_prefix("--- ") {
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
        } else if in_diff && let Some(rest) = line.strip_prefix("+++ ") {
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
        } else if in_diff && line.starts_with("@@ ") {
            // Parse @@ -old +new @@ optional_tail
            // Reset per-hunk state
            in_hunk = true;
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
        } else if in_hunk && let Some(content) = line.strip_prefix('+') {
            let hl = new_hl.get_or_insert_with(|| make_highlighter(&hint, &ps, &ts));
            let spans = hl_line(content, hl, &ps);
            // Also advance old_hl with a context-like phantom to keep state roughly in sync
            if let Some(old) = old_hl.as_mut() {
                let _ = hl_line(content, old, &ps);
            }
            write_code_line(out, "+", GREEN, &spans, BG_ADDED)?;
        } else if in_hunk && let Some(content) = line.strip_prefix('-') {
            let hl = old_hl.get_or_insert_with(|| make_highlighter(&hint, &ps, &ts));
            let spans = hl_line(content, hl, &ps);
            write_code_line(out, "-", RED, &spans, BG_REMOVED)?;
        } else if in_hunk && let Some(content) = line.strip_prefix(' ') {
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
        } else if in_hunk && line.starts_with('\\') {
            // \ No newline at end of file
            fg(out, or_, og, ob)?;
            writeln!(out, "{line}")?;
            reset(out)?;
        } else {
            in_hunk = false;
            // Try `git log` header / `--oneline` styling first; fall back to
            // an unstyled passthrough for anything else.
            reset(out)?;
            if !try_render_log_line(&line, out)? {
                writeln!(out, "{line}")?;
            }
        }
    }

    reset(out)?;
    out.flush()
}

fn run(out: &mut impl Write) -> io::Result<()> {
    let stdin = io::stdin();
    let reader = stdin.lock();
    render_lines(reader.lines(), out)
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

    #[test]
    fn test_render_lines_space_prefixed_non_diff_stays_plain() {
        let mut out = Vec::<u8>::new();
        let lines = vec![
            Ok(String::from("error: invalid option: --stats")),
            Ok(String::from("usage: git diff [<options>] [<commit>] [--] [<path>...]")),
            Ok(String::from("  --stat show diffstat instead of patch.")),
        ];
        render_lines(lines, &mut out).unwrap();

        let rendered = String::from_utf8_lossy(&out);
        assert!(
            rendered.contains("  --stat show diffstat instead of patch."),
            "space-prefixed non-diff line must pass through unchanged"
        );
        assert!(
            !rendered.contains("\x1b[48;2;30;30;46m"),
            "space-prefixed non-diff line must not be tinted as a diff context line"
        );
    }

    // ── git log styling ───────────────────────────────────────────────────────

    #[test]
    fn test_log_commit_line_renders_hash_in_peach() {
        let mut out = Vec::<u8>::new();
        assert!(
            try_render_log_line("commit 1234567890abcdef1234567890abcdef12345678", &mut out)
                .unwrap()
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("commit "), "label preserved: {s:?}");
        assert!(s.contains("1234567890abcdef1234567890abcdef12345678"));
        // PEACH = (0xfa, 0xb3, 0x87) -> 250;179;135
        assert!(s.contains("\x1b[38;2;250;179;135m"), "peach fg applied: {s:?}");
    }

    #[test]
    fn test_log_commit_line_with_ref_decoration_styles_refs() {
        let mut out = Vec::<u8>::new();
        assert!(
            try_render_log_line("commit abcdef0123456 (HEAD -> main)", &mut out).unwrap()
        );
        let s = String::from_utf8_lossy(&out);
        // YELLOW = (0xf9, 0xe2, 0xaf) -> 249;226;175
        assert!(
            s.contains("\x1b[38;2;249;226;175m"),
            "ref decoration styled yellow: {s:?}"
        );
        assert!(s.contains("(HEAD -> main)"));
    }

    #[test]
    fn test_log_author_line_splits_name_and_email() {
        let mut out = Vec::<u8>::new();
        assert!(
            try_render_log_line("Author: Phillip Carter <phil@example.com>", &mut out)
                .unwrap()
        );
        let s = String::from_utf8_lossy(&out);
        // SKY = (0x89, 0xdc, 0xeb) -> 137;220;235
        assert!(s.contains("\x1b[38;2;137;220;235m"), "name in sky: {s:?}");
        assert!(s.contains("Phillip Carter"));
        assert!(s.contains("<phil@example.com>"));
    }

    #[test]
    fn test_log_date_line_dimmed() {
        let mut out = Vec::<u8>::new();
        assert!(
            try_render_log_line("Date:   Wed Jun 24 12:00:00 2026 -0700", &mut out).unwrap()
        );
        let s = String::from_utf8_lossy(&out);
        // OVERLAY0 = (0x6c, 0x70, 0x86) -> 108;112;134
        assert!(s.contains("\x1b[38;2;108;112;134m"), "date dimmed: {s:?}");
        assert!(s.contains("Date:   Wed Jun 24"));
    }

    #[test]
    fn test_log_oneline_row_handled() {
        let mut out = Vec::<u8>::new();
        assert!(try_render_log_line("abc1234 implement nice log styling", &mut out).unwrap());
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("abc1234"));
        assert!(s.contains("implement nice log styling"));
        // Peach again for the hash.
        assert!(s.contains("\x1b[38;2;250;179;135m"));
    }

    #[test]
    fn test_log_oneline_rejects_non_hex_prefix() {
        // A regular sentence shouldn't be misread as a --oneline row.
        let mut out = Vec::<u8>::new();
        assert!(!try_render_log_line("hello world", &mut out).unwrap());
    }

    #[test]
    fn test_log_commit_rejects_short_or_non_hex() {
        let mut out = Vec::<u8>::new();
        // Too short.
        assert!(!try_render_log_line("commit 12345", &mut out).unwrap());
        // Non-hex character.
        assert!(!try_render_log_line("commit zzzzzzzzzz", &mut out).unwrap());
    }

    #[test]
    fn test_render_lines_styles_log_then_diff() {
        // Verify integration: log header followed by a diff section both get
        // their respective styling and the transition doesn't confuse state.
        let mut out = Vec::<u8>::new();
        let lines = vec![
            Ok(String::from("commit 0123456789abcdef0123456789abcdef01234567")),
            Ok(String::from("Author: Alice <alice@example.com>")),
            Ok(String::from("Date:   Wed Jun 24 12:00:00 2026 -0700")),
            Ok(String::from("")),
            Ok(String::from("    fix the thing")),
            Ok(String::from("")),
            Ok(String::from("diff --git a/foo.rs b/foo.rs")),
            Ok(String::from("--- a/foo.rs")),
            Ok(String::from("+++ b/foo.rs")),
            Ok(String::from("@@ -1,1 +1,1 @@")),
            Ok(String::from("+added")),
        ];
        render_lines(lines, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        // Peach hash from the commit header.
        assert!(s.contains("\x1b[38;2;250;179;135m"), "commit hash styled");
        // Green for the + line in the diff.
        assert!(s.contains("\x1b[38;2;166;227;161m"), "added line styled");
        // Subject body line passed through plain.
        assert!(s.contains("    fix the thing"));
    }

    #[test]
    fn test_render_lines_styles_context_only_inside_hunk() {
        let mut out = Vec::<u8>::new();
        let lines = vec![
            Ok(String::from("diff --git a/foo.rs b/foo.rs")),
            Ok(String::from("index 1111111..2222222 100644")),
            Ok(String::from("--- a/foo.rs")),
            Ok(String::from("+++ b/foo.rs")),
            Ok(String::from("@@ -1,1 +1,1 @@")),
            Ok(String::from(" context line")),
        ];
        render_lines(lines, &mut out).unwrap();

        let rendered = String::from_utf8_lossy(&out);
        assert!(
            rendered.contains("\x1b[48;2;30;30;46m"),
            "context lines inside hunks should be rendered with the context tint"
        );
    }
}
