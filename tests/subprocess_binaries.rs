use std::io::Write;
use std::path::{Path, PathBuf};
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

fn run_bin_with_env(
    name: &str,
    args: &[&str],
    input: Option<&[u8]>,
    cwd: Option<&Path>,
    envs: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(bin_path(name));
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.envs(envs.iter().copied());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().unwrap();
    if let Some(input) = input {
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        child.stdin.take();
    }
    child.wait_with_output().unwrap()
}

fn run_bin(name: &str, args: &[&str], input: Option<&str>, cwd: Option<&Path>) -> Output {
    run_bin_with_env(name, args, input.map(str::as_bytes), cwd, &[])
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

fn write_file(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
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
    let path = write_file(
        &dir,
        "sample.rs",
        "use std::fmt::Debug;\nfn main() {\n    println!(\"hi\");\n}\n",
    );

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
fn tcat_with_flags_falls_back_to_system_cat() {
    let dir = TempDir::new().unwrap();
    let path = write_file(&dir, "plain.txt", "one\ntwo\n");
    let path_arg = path.to_str().unwrap();

    let tcat_output = run_bin("tcat", &["-n", path_arg], None, None);
    let cat_output = Command::new("/bin/cat")
        .args(["-n", path_arg])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(tcat_output.status.code(), cat_output.status.code());
    assert_eq!(tcat_output.stdout, cat_output.stdout);
    assert_eq!(tcat_output.stderr, cat_output.stderr);
}

#[test]
fn tcat_renders_existing_files_and_reports_missing_ones() {
    let dir = TempDir::new().unwrap();
    let existing = write_file(&dir, "ok.rs", "fn ok() {}\n");
    let missing = dir.path().join("missing.rs");

    let output = run_bin(
        "tcat",
        &[existing.to_str().unwrap(), missing.to_str().unwrap()],
        None,
        None,
    );
    let clean = clean_stdout(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(clean.contains("ok.rs"));
    assert!(clean.contains("fn ok() {}"));
    assert!(stderr.contains("missing.rs"));
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
fn tdiff_strips_ansi_from_input_before_rendering() {
    let diff = "\
diff --git a/sample.rs b/sample.rs
--- a/sample.rs
+++ b/sample.rs
@@ -1 +1 @@
-\x1b[31mlet old = 1;\x1b[0m
+\x1b[32mlet new = 2;\x1b[0m
";
    let output = run_bin("tdiff", &[], Some(diff), None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(clean.contains("-let old = 1;"));
    assert!(clean.contains("+let new = 2;"));
    assert!(!clean.contains("31m"));
    assert!(!clean.contains("32m"));
}

#[test]
fn tdiff_new_file_diff_hides_dev_null_path() {
    let diff = "\
diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1 @@
+fn main() {}
";
    let output = run_bin("tdiff", &[], Some(diff), None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(!clean.contains("/dev/null"));
    assert!(clean.contains("+++ new.rs"));
    assert!(clean.contains("+fn main() {}"));
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
fn tjson_filter_mode_handles_final_line_without_newline() {
    let output = run_bin("tjson", &[], Some("plain\n{\"ok\":true}"), None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(clean.contains("plain"));
    assert!(clean.contains("\"ok\": true"));
    assert!(output.stdout.ends_with(b"\n"));
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
fn tjson_pty_mode_combines_stdout_and_stderr() {
    let script = "echo stdout-line; printf '%s\n' '{\"err\":true}' >&2";
    let output = run_bin("tjson", &["/bin/sh", "-lc", script], None, None);
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(clean.contains("stdout-line"));
    assert!(clean.contains("\"err\": true"));
}

#[test]
fn tjson_pty_mode_uses_env_dimensions_for_pty_size() {
    let output = run_bin_with_env(
        "tjson",
        &["/bin/sh", "-lc", "stty size"],
        None,
        None,
        &[("COLUMNS", "123"), ("LINES", "47")],
    );
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(clean.contains("47 123"), "got: {clean:?}");
}

#[test]
fn tjson_pty_mode_sets_utf8_locale() {
    let output = run_bin(
        "tjson",
        &["/bin/sh", "-lc", "printf '%s\n' \"$LANG|$LC_ALL\""],
        None,
        None,
    );
    let clean = clean_stdout(&output);

    assert!(output.status.success(), "{output:?}");
    assert!(clean.contains("en_US.UTF-8|en_US.UTF-8"), "got: {clean:?}");
}

#[test]
fn tjson_pty_mode_passes_through_invalid_utf8_bytes() {
    let output = run_bin(
        "tjson",
        &["/bin/sh", "-lc", "printf '\\200\\n'"],
        None,
        None,
    );

    assert!(output.status.success(), "{output:?}");
    assert!(
        output
            .stdout
            .windows(2)
            .any(|window| window == [0x80, b'\n']),
        "stdout bytes did not contain raw 0x80 newline sequence: {:?}",
        output.stdout
    );
}

#[test]
fn tjson_pty_mode_returns_failure_for_nonzero_child() {
    let output = run_bin("tjson", &["/bin/sh", "-lc", "exit 7"], None, None);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}
