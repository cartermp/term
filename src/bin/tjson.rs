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

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
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

// ── Core line processor (shared by both modes) ────────────────────────────────

fn process_line(
    line: &str,
    out: &mut impl Write,
    ps: &SyntaxSet,
    syntax: &syntect::parsing::SyntaxReference,
    theme: &syntect::highlighting::Theme,
    drain: &mut bool,
) {
    let trimmed = line.trim();

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                match print_highlighted(out, &pretty, ps, syntax, theme) {
                    Ok(()) => return,
                    Err(_) => { *drain = true; return; }
                }
            }
        }
    }

    // Non-JSON or failed parse: pass through unchanged.
    if writeln!(out, "{line}").is_err() {
        *drain = true;
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

    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if drain { continue; }
        process_line(&line, &mut out, ps, syntax, theme, &mut drain);
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
    let cols: u16 = std::env::var("COLUMNS").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(220);
    let rows: u16 = std::env::var("LINES").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
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

        // Process all complete lines (ending with \n) in the buffer.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes = buf.drain(..=pos).collect::<Vec<u8>>();
            // Strip trailing \n and \r\n.
            if line_bytes.last() == Some(&b'\n') { line_bytes.pop(); }
            if line_bytes.last() == Some(&b'\r') { line_bytes.pop(); }

            if drain { continue; }

            // Try to interpret as UTF-8 for JSON detection; fall back to raw write.
            match std::str::from_utf8(&line_bytes) {
                Ok(line) => process_line(line, &mut out, ps, syntax, theme, &mut drain),
                Err(_) => {
                    // Not valid UTF-8 — write raw bytes unchanged.
                    let _ = out.write_all(&line_bytes);
                    let _ = out.write_all(b"\n");
                }
            }
        }
    }

    // Flush any remaining bytes that had no trailing newline.
    if !buf.is_empty() && !drain {
        match std::str::from_utf8(&buf) {
            Ok(line) => process_line(line, &mut out, ps, syntax, theme, &mut drain),
            Err(_) => { let _ = out.write_all(&buf); }
        }
    }

    let exit_code = match child.wait() {
        Ok(status) => if status.success() { 0 } else { 1 },
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
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 3, "expected multi-line pretty output, got: {out:?}");
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
        let would_parse = trimmed.starts_with('{')
            && serde_json::from_str::<serde_json::Value>(trimmed).is_ok();
        assert!(!would_parse, "partial JSON must fall through to passthrough");
    }
}
