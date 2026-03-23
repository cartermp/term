use std::io::{self, Read, Write};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

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

// ── Header ────────────────────────────────────────────────────────────────────

fn print_header(out: &mut impl Write, path: &str, lang: &str) -> io::Result<()> {
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

fn highlight_file(path: &str, ps: &SyntaxSet, ts: &ThemeSet) -> io::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;

    let syntax = ps
        .find_syntax_for_file(path)
        .unwrap_or(None)
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

    print_header(&mut out, path, lang)?;

    let total_lines = content.lines().count();
    let gutter_width = total_lines.to_string().len().max(2);
    let (or_, og, ob) = OVERLAY0;
    let (sfr, sfg, sfb) = SURFACE1;

    for (i, line) in LinesWithEndings::from(&content).enumerate() {
        let lineno = i + 1;

        // Gutter: "  N │ "
        out.write_all(b"  ")?;
        fg(&mut out, or_, og, ob)?;
        write!(out, "{:>width$}", lineno, width = gutter_width)?;
        out.write_all(b" ")?;
        fg(&mut out, sfr, sfg, sfb)?;
        out.write_all("│".as_bytes())?;
        fg(&mut out, or_, og, ob)?;
        out.write_all(b" ")?;
        reset(&mut out)?;

        // Highlighted content
        let ranges = h.highlight_line(line, ps).unwrap_or_default();
        for (style, text) in &ranges {
            // Strip trailing newline from the last span so reset doesn't leave colour on blank line
            let t = if let Some(stripped) = text.strip_suffix('\n') {
                stripped
            } else {
                text
            };
            if !t.is_empty() {
                write_span(&mut out, *style, t)?;
            }
        }
        writeln!(&mut out)?;
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
        let _ = io::stdin().read_to_end(&mut buf);
        let _ = io::stdout().write_all(&buf);
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
    for file in &args {
        if let Err(e) = highlight_file(file, &ps, &ts) {
            eprintln!("cat: {e}");
            exit_code = 1;
        }
    }
    std::process::exit(exit_code);
}
