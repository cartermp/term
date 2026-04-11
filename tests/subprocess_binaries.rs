use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn bin_path(name: &str) -> &'static str {
    match name {
        "tcat" => env!("CARGO_BIN_EXE_tcat"),
        "tdiff" => env!("CARGO_BIN_EXE_tdiff"),
        "tjson" => env!("CARGO_BIN_EXE_tjson"),
        _ => panic!("unknown binary: {name}"),
    }
}

fn run_bin(name: &str, args: &[&str], input: Option<&str>, cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(bin_path(name));
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.stdin.take();
    }
    child.wait_with_output().unwrap()
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

fn clean_stdout(output: &Output) -> String {
    strip_ansi(&String::from_utf8_lossy(&output.stdout).replace('\r', ""))
}

#[test]
fn tcat_passthroughs_stdin_without_args() {
    let output = run_bin("tcat", &[], Some("hello\nworld\n"), None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello\nworld\n");
}

#[test]
fn tcat_highlights_requested_range() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sample.rs");
    std::fs::write(
        &path,
        "use std::fmt::Debug;\nfn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();

    let arg = format!("{}:2-3", path.display());
    let output = run_bin("tcat", &[&arg], None, None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\x1b["));
    assert!(clean.contains("sample.rs"));
    assert!(clean.contains("Rust"));
    assert!(clean.contains("2 │ fn main() {"));
    assert!(clean.contains("3 │     println!(\"hi\");"));
    assert!(!clean.contains("1 │ use std::fmt::Debug;"));
}

#[test]
fn tdiff_renders_unified_diff_with_color() {
    let diff = "\
diff --git a/sample.rs b/sample.rs
index 1111111..2222222 100644
--- a/sample.rs
+++ b/sample.rs
@@ -1,2 +1,2 @@
-let old = 1;
+let new = 2;
 unchanged();
";
    let output = run_bin("tdiff", &[], Some(diff), None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\x1b["));
    assert!(clean.contains("diff --git a/sample.rs b/sample.rs"));
    assert!(clean.contains("--- sample.rs"));
    assert!(clean.contains("+++ sample.rs"));
    assert!(clean.contains("-let old = 1;"));
    assert!(clean.contains("+let new = 2;"));
    assert!(clean.contains(" unchanged();"));
}

#[test]
fn tjson_filter_mode_prettifies_json_and_preserves_plain_text() {
    let output = run_bin(
        "tjson",
        &[],
        Some("server ready\n{\"ok\":true,\"answer\":42}\n"),
        None,
    );
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\x1b["));
    assert!(clean.contains("server ready"));
    assert!(clean.contains("\"ok\": true"));
    assert!(clean.contains("\"answer\": 42"));
}

#[test]
fn tjson_pty_mode_uses_real_tty_and_current_dir() {
    let dir = TempDir::new().unwrap();
    let script =
        "if [ -t 1 ]; then echo tty; else echo notty; fi; pwd; printf '%s\n' '{\"ok\":true}'";
    let output = run_bin("tjson", &["/bin/sh", "-lc", script], None, Some(dir.path()));
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\x1b["));
    assert!(clean.contains("tty"));
    assert!(clean.contains(dir.path().to_str().unwrap()));
    assert!(clean.contains("\"ok\": true"));
}

#[test]
fn tjson_pty_mode_returns_failure_for_nonzero_child() {
    let output = run_bin("tjson", &["/bin/sh", "-lc", "exit 7"], None, None);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}
