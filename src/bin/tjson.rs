//! tjson — streaming JSON prettifier with syntax highlighting.
//!
//! Two modes:
//!   some-cmd | tjson          — filter mode: reads stdin line by line
//!   tjson pnpm dev [args…]    — PTY mode: runs the command in a PTY so it
//!                               sees a real terminal on stdout/stderr, then
//!                               filters its combined output the same way.
//!
//! Lines that parse as JSON objects or arrays are pretty-printed with 24-bit
//! ANSI colour (syntect, base16-ocean.dark theme). All other lines pass
//! through unchanged.
//!
//! Usage:
//!   pnpm dev | tjson
//!   json pnpm dev            # via the shell alias (preferred — preserves TTY)

use std::io::{self, BufRead, Read, Write};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// ── ANSI helpers ──────────────────────────────────────────────────────────────

fn fg(out: &mut impl Write, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(out, "\x1b[38;2;{r};{g};{b}m")
}
fn reset(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[0m")
}
fn write_span(out: &mut impl Write, style: Style, text: &str) -> io::Result<()> {
    let s = style.foreground;
    fg(out, s.r, s.g, s.b)?;
    if style.font_style.contains(FontStyle::BOLD) {
        out.write_all(b"\x1b[1m")?;
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out.write_all(b"\x1b[3m")?;
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out.write_all(b"\x1b[4m")?;
    }
    out.write_all(text.as_bytes())?;
    reset(out)
}

// ── Pretty-print one JSON value with syntax highlighting ──────────────────────

fn print_highlighted(
    out: &mut impl Write,
    pretty: &str,
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
) -> io::Result<()> {
    let mut h = HighlightLines::new(syntax, theme);
    for line in LinesWithEndings::from(pretty) {
        let ranges = h.highlight_line(line, ps).unwrap_or_default();
        for (style, text) in &ranges {
            let t = text.strip_suffix('\n').unwrap_or(text);
            let t = t.strip_suffix('\r').unwrap_or(t);
            if !t.is_empty() {
                write_span(out, *style, t)?;
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

// ── Core line processor (shared by both modes) ────────────────────────────────

fn try_print_json(
    line: &str,
    out: &mut impl Write,
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
) -> io::Result<bool> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Ok(false);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Ok(false);
    };
    let pretty = serde_json::to_string_pretty(&value)?;
    print_highlighted(out, &pretty, ps, syntax, theme)?;
    Ok(true)
}

#[cfg(test)]
fn process_line(
    line: &str,
    out: &mut impl Write,
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
    drain: &mut bool,
) {
    match try_print_json(line, out, ps, syntax, theme) {
        Ok(true) => return,
        Ok(false) => {}
        Err(_) => {
            *drain = true;
            return;
        }
    }

    // Non-JSON or failed parse: pass through unchanged.
    if writeln!(out, "{line}").is_err() {
        *drain = true;
    }
}

fn process_record(
    record: &[u8],
    out: &mut impl Write,
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
    drain: &mut bool,
) {
    if *drain {
        return;
    }

    let mut content_end = record.len();
    if content_end > 0 && record[content_end - 1] == b'\n' {
        content_end -= 1;
    }
    if content_end > 0 && record[content_end - 1] == b'\r' {
        content_end -= 1;
    }

    match std::str::from_utf8(&record[..content_end]) {
        Ok(line) => match try_print_json(line, out, ps, syntax, theme) {
            Ok(true) => {}
            Ok(false) => {
                if out.write_all(record).is_err() {
                    *drain = true;
                }
            }
            Err(_) => *drain = true,
        },
        Err(_) => {
            // Invalid UTF-8 is not JSON, but it is still valid command output.
            // Preserve the original bytes and line ending exactly.
            if out.write_all(record).is_err() {
                *drain = true;
            }
        }
    }
}

// ── Filter mode: read from stdin ──────────────────────────────────────────────

fn run_filter(
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
) {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut drain = false;

    let mut input = io::stdin().lock();
    let mut record = Vec::new();
    loop {
        record.clear();
        match input.read_until(b'\n', &mut record) {
            Ok(0) | Err(_) => break,
            Ok(_) => process_record(&record, &mut out, ps, syntax, theme, &mut drain),
        }
    }
    let _ = out.flush();
}

// ── PTY mode: run a command so it sees a real terminal ────────────────────────

fn run_pty(
    args: &[String],
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
) {
    // Inherit terminal dimensions if available.
    let cols: u16 = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(220);
    let rows: u16 = std::env::var("LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    let mut cmd = CommandBuilder::new(&args[0]);
    for arg in &args[1..] {
        cmd.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // Force UTF-8 locale so multi-byte characters aren't re-encoded via Mac Roman.
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("LC_ALL", "en_US.UTF-8");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn failed");
    drop(pair.slave); // child owns the slave end

    // PTY master gives us combined stdout+stderr from the child.
    let mut reader = pair.master.try_clone_reader().expect("clone reader");

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut drain = false;

    // Read raw bytes to avoid String round-trips that can corrupt multi-byte
    // sequences when the locale isn't UTF-8.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);

        // Process complete lines without allocating and shifting the remainder
        // once per line. Any partial trailing record stays in `buf`.
        let mut consumed = 0usize;
        while let Some(relative_end) = buf[consumed..].iter().position(|&b| b == b'\n') {
            let end = consumed + relative_end + 1;
            process_record(&buf[consumed..end], &mut out, ps, syntax, theme, &mut drain);
            consumed = end;
        }
        if consumed > 0 {
            buf.copy_within(consumed.., 0);
            buf.truncate(buf.len() - consumed);
        }
    }

    // Flush any remaining bytes that had no trailing newline.
    if !buf.is_empty() && !drain {
        process_record(&buf, &mut out, ps, syntax, theme, &mut drain);
    }

    let exit_code = match child.wait() {
        Ok(status) => status.exit_code().min(255) as i32,
        Err(_) => 1,
    };
    std::process::exit(exit_code);
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let syntax = ps
        .find_syntax_by_extension("json")
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let theme = ["base16-ocean.dark", "Solarized (dark)"]
        .iter()
        .find_map(|n| ts.themes.get(*n))
        .or_else(|| ts.themes.values().next())
        .expect("syntect has no themes");

    if args.is_empty() {
        run_filter(&ps, syntax, theme);
    } else {
        run_pty(&args, &ps, syntax, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighted(json: &str) -> String {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let syntax = ps
            .find_syntax_by_extension("json")
            .unwrap_or_else(|| ps.find_syntax_plain_text());
        let theme = ts.themes.values().next().expect("no theme");
        let val: serde_json::Value = serde_json::from_str(json).unwrap();
        let pretty = serde_json::to_string_pretty(&val).unwrap();
        let mut buf = Vec::new();
        print_highlighted(&mut buf, &pretty, &ps, syntax, theme).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_json_object_contains_keys() {
        let out = highlighted(r#"{"level":30,"msg":"hello"}"#);
        assert!(out.contains("level"), "key 'level' missing from output");
        assert!(out.contains("msg"), "key 'msg' missing from output");
        assert!(out.contains("hello"), "string value missing from output");
        assert!(out.contains("30"), "number value missing from output");
    }

    #[test]
    fn test_json_array_rendered() {
        let out = highlighted(r#"[1,2,3]"#);
        assert!(out.contains('1'.to_string().as_str()));
        assert!(out.contains('3'.to_string().as_str()));
    }

    #[test]
    fn test_output_has_ansi_escapes() {
        let out = highlighted(r#"{"x":1}"#);
        assert!(
            out.contains("\x1b["),
            "expected ANSI escape sequences in output"
        );
    }

    #[test]
    fn test_pretty_printed_multiline() {
        let out = highlighted(r#"{"a":1,"b":2}"#);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected multi-line pretty output, got: {out:?}"
        );
    }

    #[test]
    fn test_non_json_passthrough() {
        let line = "Next.js 16.2.0 (Turbopack)";
        let trimmed = line.trim();
        let would_parse = (trimmed.starts_with('{') || trimmed.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(trimmed).is_ok();
        assert!(!would_parse, "plain text must not be treated as JSON");
    }

    #[test]
    fn test_partial_json_not_parsed() {
        let line = r#"{"incomplete":"#;
        let trimmed = line.trim();
        let would_parse =
            trimmed.starts_with('{') && serde_json::from_str::<serde_json::Value>(trimmed).is_ok();
        assert!(
            !would_parse,
            "partial JSON must fall through to passthrough"
        );
    }

    // ── process_line passthrough ──────────────────────────────────────────────

    fn process_output(line: &str) -> String {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let syntax = ps
            .find_syntax_by_extension("json")
            .unwrap_or_else(|| ps.find_syntax_plain_text());
        let theme = ts.themes.values().next().expect("no theme");
        let mut buf = Vec::new();
        let mut drain = false;
        process_line(line, &mut buf, &ps, syntax, theme, &mut drain);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_invalid_utf8_record_passes_through_byte_for_byte() {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let syntax = ps
            .find_syntax_by_extension("json")
            .unwrap_or_else(|| ps.find_syntax_plain_text());
        let theme = ts.themes.values().next().expect("no theme");
        let record = b"raw \x80 bytes\r\n";
        let mut out = Vec::new();
        let mut drain = false;
        process_record(record, &mut out, &ps, syntax, theme, &mut drain);
        assert_eq!(out, record);
        assert!(!drain);
    }

    #[test]
    fn test_number_passes_through_unchanged() {
        let out = process_output("42");
        assert!(out.contains("42"), "number must pass through: {out:?}");
        assert!(
            !out.contains("\x1b["),
            "number must not be syntax-highlighted"
        );
    }

    #[test]
    fn test_null_passes_through_unchanged() {
        let out = process_output("null");
        assert!(out.contains("null"), "null literal must pass through");
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn test_boolean_passes_through_unchanged() {
        let out = process_output("true");
        assert!(out.contains("true"), "boolean must pass through");
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn test_plain_text_passes_through_unchanged() {
        let line = "Starting server on port 3000";
        let out = process_output(line);
        assert!(out.contains(line), "plain text must pass through unchanged");
    }

    #[test]
    fn test_invalid_json_braces_passes_through() {
        // Looks like JSON but isn't — unquoted keys.
        let line = "{key: value}";
        let out = process_output(line);
        assert!(
            out.contains("{key: value}"),
            "invalid JSON must pass through: {out:?}"
        );
    }

    #[test]
    fn test_json_with_trailing_garbage_passes_through() {
        // serde_json rejects trailing non-whitespace after a valid value.
        let line = r#"{"a": 1} not json"#;
        let out = process_output(line);
        assert!(
            out.contains(r#"{"a": 1} not json"#),
            "JSON with trailing garbage must pass through"
        );
    }

    #[test]
    fn test_empty_object_prettified() {
        let out = process_output("{}");
        assert!(
            out.contains("{}") || out.contains('{'),
            "empty object must be prettified"
        );
        assert!(
            out.contains("\x1b["),
            "prettified output must have ANSI escapes"
        );
    }

    #[test]
    fn test_empty_array_prettified() {
        let out = process_output("[]");
        assert!(
            out.contains("[]") || out.contains('['),
            "empty array must be prettified"
        );
        assert!(
            out.contains("\x1b["),
            "prettified output must have ANSI escapes"
        );
    }

    #[test]
    fn test_leading_whitespace_stripped_before_parse() {
        // process_line calls `line.trim()` before JSON detection, so
        // "   {}" is treated as "{}" and gets prettified.
        let out = process_output(r#"   {"x": 1}"#);
        assert!(
            out.contains("\x1b["),
            "JSON with leading spaces must be prettified"
        );
        // The raw leading spaces must NOT appear in prettified output.
        assert!(
            !out.starts_with("   {"),
            "leading spaces must be stripped from prettified output"
        );
    }

    // ── Additional process_line / process_output tests ────────────────────────

    #[test]
    fn test_empty_line_passes_through() {
        let out = process_output("");
        // An empty line is not JSON — it must be echoed (as a bare newline).
        assert_eq!(out, "\n", "empty line must be passed through as a newline");
    }

    #[test]
    fn test_whitespace_only_line_passes_through() {
        let out = process_output("   ");
        // Whitespace-only lines cannot start with '{' or '[' after trimming.
        assert!(
            out.contains("   "),
            "whitespace-only line must pass through unchanged"
        );
    }

    #[test]
    fn test_unicode_json_object_prettified() {
        let out = process_output(r#"{"emoji":"🦀","name":"裁"}"#);
        assert!(out.contains("\x1b["), "unicode JSON must be prettified");
        assert!(out.contains("emoji"), "key must appear in output");
    }

    #[test]
    fn test_deeply_nested_json_prettified() {
        let out = process_output(r#"{"a":{"b":{"c":{"d":42}}}}"#);
        assert!(
            out.contains("\x1b["),
            "deeply nested JSON must be prettified"
        );
        assert!(out.contains("42"), "leaf value must appear in output");
    }

    #[test]
    fn test_json_array_of_objects_prettified() {
        let out = process_output(r#"[{"id":1},{"id":2}]"#);
        assert!(out.contains("\x1b["), "array of objects must be prettified");
        // syntect may split tokens at quote boundaries; check the key text only.
        assert!(out.contains("id"), "key name must appear in output");
    }

    #[test]
    fn test_line_starting_with_brace_but_invalid_json_passes_through() {
        let line = "{not valid json at all";
        let out = process_output(line);
        assert!(
            out.contains(line),
            "invalid JSON starting with '{{' must pass through"
        );
    }

    #[test]
    fn test_line_starting_with_bracket_but_invalid_json_passes_through() {
        let line = "[1, 2, broken";
        let out = process_output(line);
        assert!(
            out.contains(line),
            "invalid JSON starting with '[' must pass through"
        );
    }

    #[test]
    fn test_json_with_null_value_prettified() {
        let out = process_output(r#"{"key":null}"#);
        assert!(
            out.contains("null"),
            "null value must appear in prettified output"
        );
    }

    #[test]
    fn test_json_with_boolean_values_prettified() {
        let out = process_output(r#"{"ok":true,"fail":false}"#);
        assert!(out.contains("true"));
        assert!(out.contains("false"));
    }

    #[test]
    fn test_multiple_keys_each_on_own_line_in_output() {
        let out = process_output(r#"{"a":1,"b":2,"c":3}"#);
        // serde_json::to_string_pretty puts each key on its own line.
        let newline_count = out.chars().filter(|&c| c == '\n').count();
        assert!(
            newline_count >= 3,
            "prettified 3-key object must have ≥3 newlines, got {newline_count}"
        );
    }
}
