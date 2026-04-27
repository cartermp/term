use serde_json::Value;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const RELEASE_API_URL: &str = "https://api.github.com/repos/cartermp/term/releases/latest";
const RELEASE_ASSET_NAME: &str = "Term.app.zip";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub zip_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    NoUpdateNeeded {
        current_version: String,
        latest_version: String,
        comparison: Ordering,
    },
    UpdateAvailable {
        current_version: String,
        release: ReleaseInfo,
    },
}

pub fn current_version() -> String {
    bundle_version_from_current_exe().unwrap_or_else(compiled_version)
}

pub fn display_version(version: &str) -> String {
    let normalized = normalize_version(version);
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        format!("v{normalized}")
    }
}

pub fn check_for_updates() -> Result<UpdateCheck, String> {
    let current_version = current_version();
    let release = fetch_latest_release()?;
    let comparison = compare_versions(&current_version, &release.version);
    if comparison == Ordering::Less {
        Ok(UpdateCheck::UpdateAvailable {
            current_version,
            release,
        })
    } else {
        Ok(UpdateCheck::NoUpdateNeeded {
            current_version,
            latest_version: release.version,
            comparison,
        })
    }
}

pub fn spawn_background_update(release: &ReleaseInfo) -> Result<(), String> {
    let app_bundle = current_app_bundle()?;
    let work_dir = std::env::temp_dir().join(format!(
        "term-update-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("failed to create updater workspace: {e}"))?;

    let script_path = work_dir.join("install-update.sh");
    let log_path = work_dir.join("update.log");
    std::fs::write(&script_path, build_update_script())
        .map_err(|e| format!("failed to write updater script: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .map_err(|e| format!("failed to inspect updater script: {e}"))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&script_path, perms)
            .map_err(|e| format!("failed to mark updater script executable: {e}"))?;
    }

    Command::new("/bin/bash")
        .arg(&script_path)
        .arg(std::process::id().to_string())
        .arg(&app_bundle)
        .arg(&release.tag_name)
        .arg(&release.zip_url)
        .arg(&log_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to launch updater helper: {e}"))?;

    Ok(())
}

fn compiled_version() -> String {
    option_env!("TERM_RELEASE_VERSION")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .trim()
        .to_string()
}

fn fetch_latest_release() -> Result<ReleaseInfo, String> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: term-updater",
            RELEASE_API_URL,
        ])
        .output()
        .map_err(|e| format!("failed to fetch latest release metadata: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("latest release lookup failed with status {}", output.status)
        } else {
            format!("latest release lookup failed: {stderr}")
        });
    }

    parse_latest_release_json(&String::from_utf8_lossy(&output.stdout))
}

fn parse_latest_release_json(json: &str) -> Result<ReleaseInfo, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid GitHub release payload: {e}"))?;
    let tag_name = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "latest release payload is missing tag_name".to_string())?;
    let zip_url = value
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                (asset.get("name").and_then(Value::as_str) == Some(RELEASE_ASSET_NAME))
                    .then(|| {
                        asset
                            .get("browser_download_url")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .flatten()
            })
        })
        .ok_or_else(|| format!("latest release does not include {RELEASE_ASSET_NAME}"))?;

    Ok(ReleaseInfo {
        tag_name: tag_name.to_string(),
        version: normalize_version(tag_name),
        zip_url,
    })
}

fn current_app_bundle() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("failed to locate running executable: {e}"))?;
    app_bundle_from_exe_path(&exe).ok_or_else(|| {
        format!(
            "self-update only works when Term is running from a .app bundle (current executable: {}).",
            exe.display()
        )
    })
}

fn bundle_version_from_current_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let app_bundle = app_bundle_from_exe_path(&exe)?;
    let plist = std::fs::read_to_string(app_bundle.join("Contents/Info.plist")).ok()?;
    plist_string_value(&plist, "CFBundleShortVersionString")
        .or_else(|| plist_string_value(&plist, "CFBundleVersion"))
        .map(|value| normalize_version(&value))
        .filter(|value| !value.is_empty())
}

fn app_bundle_from_exe_path(path: &Path) -> Option<PathBuf> {
    let macos_dir = path.parent()?;
    if macos_dir.file_name() != Some(OsStr::new("MacOS")) {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name() != Some(OsStr::new("Contents")) {
        return None;
    }
    let app_bundle = contents_dir.parent()?;
    (app_bundle.extension() == Some(OsStr::new("app"))).then(|| app_bundle.to_path_buf())
}

fn plist_string_value(plist: &str, key: &str) -> Option<String> {
    let key_marker = format!("<key>{key}</key>");
    let key_idx = plist.find(&key_marker)?;
    let tail = &plist[key_idx + key_marker.len()..];
    let string_start = tail.find("<string>")?;
    let tail = &tail[string_start + "<string>".len()..];
    let string_end = tail.find("</string>")?;
    Some(tail[..string_end].trim().to_string())
}

fn normalize_version(version: &str) -> String {
    let base = version.trim().trim_start_matches(['v', 'V']);
    let base = base
        .split_once('+')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base);
    let base = base
        .split_once('-')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base);
    base.trim().to_string()
}

fn compare_versions(a: &str, b: &str) -> Ordering {
    let a_norm = normalize_version(a);
    let b_norm = normalize_version(b);
    match (parse_version_parts(&a_norm), parse_version_parts(&b_norm)) {
        (Some(a_parts), Some(b_parts)) => compare_version_parts(&a_parts, &b_parts),
        _ => a_norm.cmp(&b_norm),
    }
}

fn parse_version_parts(version: &str) -> Option<Vec<u64>> {
    let mut parts = Vec::new();
    for piece in version.split('.') {
        if piece.is_empty() {
            return None;
        }
        parts.push(piece.parse().ok()?);
    }
    Some(parts)
}

fn compare_version_parts(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    for idx in 0..len {
        let a_part = a.get(idx).copied().unwrap_or(0);
        let b_part = b.get(idx).copied().unwrap_or(0);
        match a_part.cmp(&b_part) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn build_update_script() -> &'static str {
    r#"#!/bin/bash
set -euo pipefail

PID="$1"
APP_DST="$2"
VERSION="$3"
ZIP_URL="$4"
LOG_PATH="$5"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"

cleanup() {
  rm -rf "$TMP" "$SCRIPT_DIR"
}
trap cleanup EXIT

report_failure() {
  /usr/bin/osascript - "$LOG_PATH" <<'APPLESCRIPT' >/dev/null 2>&1 || true
on run argv
  display alert "Term update failed" message ("See " & item 1 of argv & " for details.") as critical
end run
APPLESCRIPT
}

{
  echo "Downloading Term ${VERSION}..."
  /usr/bin/curl -fL --silent --show-error "$ZIP_URL" -o "$TMP/Term.app.zip"
  /usr/bin/ditto -x -k "$TMP/Term.app.zip" "$TMP"
  if [ ! -d "$TMP/Term.app" ]; then
    echo "error: downloaded archive did not contain Term.app"
    exit 1
  fi
  /usr/bin/xattr -cr "$TMP/Term.app" || true

  if /bin/kill -0 "$PID" 2>/dev/null; then
    /bin/kill "$PID" 2>/dev/null || true
  fi
  while /bin/kill -0 "$PID" 2>/dev/null; do
    /bin/sleep 0.2
  done

  /bin/rm -rf "$APP_DST"
  /usr/bin/ditto "$TMP/Term.app" "$APP_DST"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$APP_DST" 2>/dev/null || true
  /usr/bin/open -a "$APP_DST"
} >>"$LOG_PATH" 2>&1 || report_failure
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_latest_release_asset_url() {
        let release = parse_latest_release_json(
            r#"{
                "tag_name": "v1.3.1",
                "assets": [
                    {"name": "install.sh", "browser_download_url": "https://example/install.sh"},
                    {"name": "Term.app.zip", "browser_download_url": "https://example/Term.app.zip"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(release.tag_name, "v1.3.1");
        assert_eq!(release.version, "1.3.1");
        assert_eq!(release.zip_url, "https://example/Term.app.zip");
    }

    #[test]
    fn compares_semverish_versions_numerically() {
        assert_eq!(compare_versions("1.3.1", "v1.3.1"), Ordering::Equal);
        assert_eq!(compare_versions("1.3.2", "1.3.1"), Ordering::Greater);
        assert_eq!(compare_versions("1.3", "1.3.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Ordering::Greater);
    }

    #[test]
    fn finds_app_bundle_from_executable_path() {
        let exe = Path::new("/Applications/Term.app/Contents/MacOS/term");
        assert_eq!(
            app_bundle_from_exe_path(exe),
            Some(PathBuf::from("/Applications/Term.app"))
        );
    }

    #[test]
    fn extracts_bundle_version_from_plist_xml() {
        let plist = r#"
            <dict>
              <key>CFBundleName</key><string>Term</string>
              <key>CFBundleShortVersionString</key><string>1.3.1</string>
            </dict>
        "#;
        assert_eq!(
            plist_string_value(plist, "CFBundleShortVersionString").as_deref(),
            Some("1.3.1")
        );
    }
}
