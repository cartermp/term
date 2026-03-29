//! tjson — streaming JSON prettifier with syntax highlighting.
//!
//! Reads stdin line by line. Lines that parse as JSON objects or arrays are
//! pretty-printed with 24-bit ANSI colour (syntect, base16-ocean.dark theme).
//! All other lines pass through unchanged.
//!
//! Usage:
//!   pnpm dev | tjson
//!   some-command | tjson

use std::io::{self, BufRead, Write};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// ── ANSI helpers (mirrors tcat) ───────────────────────────────────────────────

fn fg(out: &mut impl Write, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(out, "\x1b[38;2;{r};{g};{b}m")
}
fn reset(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[0m")
}
fn write_span(out: &mut impl Write, style: Style, text: &str) -> io::Result<()> {
    let s = style.foreground;
    fg(out, s.r, s.g, s.b)?;
    if style.font_style.contains(FontStyle::BOLD)      { out.write_all(b"\x1b[1m")?; }
    if style.font_style.contains(FontStyle::ITALIC)    { out.write_all(b"\x1b[3m")?; }
    if style.font_style.contains(FontStyle::UNDERLINE) { out.write_all(b"\x1b[4m")?; }
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

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
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

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    // When stdout breaks we switch to drain mode: keep reading stdin until EOF
    // so the upstream process never sees a broken pipe (EPIPE). Node.js in
    // particular crashes with an uncaughtException on unhandled EPIPE.
    let mut drain = false;

    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if drain { continue; }

        let trimmed = line.trim();

        // Only attempt to parse lines that look like JSON objects or arrays.
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                    match print_highlighted(&mut out, &pretty, &ps, syntax, theme) {
                        Ok(()) => continue,
                        Err(_) => { drain = true; continue; }
                    }
                }
            }
        }

        // Non-JSON or failed parse: pass through unchanged.
        if writeln!(out, "{line}").is_err() {
            drain = true;
        }
    }

    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighted(json: &str) -> String {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let syntax = ps.find_syntax_by_extension("json")
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
        assert!(out.contains("level"),  "key 'level' missing from output");
        assert!(out.contains("msg"),    "key 'msg' missing from output");
        assert!(out.contains("hello"),  "string value missing from output");
        assert!(out.contains("30"),     "number value missing from output");
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
        assert!(out.contains("\x1b["), "expected ANSI escape sequences in output");
    }

    #[test]
    fn test_pretty_printed_multiline() {
        let out = highlighted(r#"{"a":1,"b":2}"#);
        // Pretty-printing should produce at least 3 lines: opening brace, fields, closing brace
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 3, "expected multi-line pretty output, got: {out:?}");
    }

    // parse_check: non-JSON lines should not be accidentally parsed
    #[test]
    fn test_non_json_passthrough() {
        // Simulate the passthrough branch directly.
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
        let would_parse = trimmed.starts_with('{')
            && serde_json::from_str::<serde_json::Value>(trimmed).is_ok();
        assert!(!would_parse, "partial JSON must fall through to passthrough");
    }
}
