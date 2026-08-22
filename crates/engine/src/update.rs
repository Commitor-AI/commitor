use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Commitor-AI/commitor/releases/latest";

/// Seconds between automatic update checks (24 hours).
pub const UPDATE_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Total time allowed for a release check. The CLI blocks on this
/// check, so it must never hang.
const RELEASE_CHECK_TIMEOUT_SECS: u64 = 5;

/// Marker file (inside [`config_dir`]) holding the version of a
/// release that must be installed via `commitor update` before the
/// CLI may be used again.
const PENDING_UPDATE_FILE: &str = "pending_update";

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub html_url: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// The version of the crate currently running.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn user_agent() -> String {
    format!("commitor/{}", current_version())
}

/// Query the GitHub API for the latest published release.
pub fn check_latest_release() -> Result<ReleaseInfo> {
    let client = Client::builder()
        .timeout(Duration::from_secs(RELEASE_CHECK_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("failed to reach the GitHub API")?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .unwrap_or_else(|_| "<could not read response body>".to_string());
        bail!(
            "GitHub API returned HTTP {}: {}",
            status.as_u16(),
            body_text
        );
    }

    response
        .json()
        .context("failed to parse GitHub release response")
}

/// Return true if `latest_tag` is a strictly greater version than `current`.
///
/// Both inputs may carry a leading "v" (e.g. "v0.2.0"). Unparseable
/// versions compare as "not newer" so a bad release tag never bricks
/// the updater.
pub fn is_newer(latest_tag: &str, current: &str) -> bool {
    let parse = |s: &str| Version::parse(s.trim().strip_prefix('v').unwrap_or(s.trim()));
    match (parse(latest_tag), parse(current)) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

/// The target-triple part of the expected release asset name for this
/// machine, e.g. `x86_64-apple-darwin`.
///
/// Must match the naming convention used by the release workflow:
/// `commitor-{target-triple}` (+ `.exe` on Windows).
fn target_triple() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", arch) => format!("{arch}-unknown-linux-gnu"),
        ("macos", arch) => format!("{arch}-apple-darwin"),
        ("windows", arch) => format!("{arch}-pc-windows-msvc"),
        (os, arch) => format!("{arch}-{os}"),
    }
}

/// The release asset name this binary expects to download,
/// e.g. `commitor-x86_64-unknown-linux-gnu`.
pub fn expected_asset_name() -> String {
    if cfg!(windows) {
        format!("commitor-{}.exe", target_triple())
    } else {
        format!("commitor-{}", target_triple())
    }
}

/// Pick the release asset built for the current OS/architecture.
pub fn find_matching_asset(release: &ReleaseInfo) -> Result<&ReleaseAsset> {
    let wanted = expected_asset_name();
    release
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no asset named `{wanted}` — download it manually from {}",
                release.tag_name,
                release.html_url
            )
        })
}

/// Download the asset at `asset_url` and replace the running binary.
///
/// The file is downloaded next to the current executable (same
/// filesystem, so the final swap is an atomic rename). On Unix,
/// renaming over a running executable is allowed. On Windows, the
/// running exe is locked, so the old binary is moved to `<exe>.old`
/// first; the stale copy is deleted by [`cleanup_stale_old_binary`] on
/// the next run.
pub fn download_and_replace_binary(asset_url: &str) -> Result<()> {
    let exe_path = std::env::current_exe().context("failed to locate the running executable")?;
    let exe_dir = exe_path
        .parent()
        .context("running executable has no parent directory")?
        .to_path_buf();

    let mut response = Client::new()
        .get(asset_url)
        .header("User-Agent", user_agent())
        .send()
        .context("failed to start downloading the new binary")?;

    let status = response.status();
    if !status.is_success() {
        bail!("download returned HTTP {}", status.as_u16());
    }

    let tmp_path = exe_dir.join(format!(".commitor-update-{}.tmp", std::process::id()));
    {
        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        response
            .copy_to(&mut file)
            .context("download failed midway")?;
    }

    make_executable(&tmp_path)?;

    #[cfg(unix)]
    fs::rename(&tmp_path, &exe_path).with_context(|| {
        format!(
            "failed to replace {} with the downloaded binary",
            exe_path.display()
        )
    })?;

    #[cfg(windows)]
    {
        let old_path = suffixed_path(&exe_path, ".old");
        // A previous failed update could leave one behind.
        let _ = fs::remove_file(&old_path);
        fs::rename(&exe_path, &old_path)
            .context("failed to move the current binary aside before replacing it")?;
        fs::rename(&tmp_path, &exe_path).with_context(|| {
            format!("failed to install the new binary at {}", exe_path.display())
        })?;
    }

    Ok(())
}

/// Delete the `.old` binary left behind by a Windows self-update.
///
/// No-op on other platforms. Call once at CLI startup.
pub fn cleanup_stale_old_binary() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(suffixed_path(&exe, ".old"));
    }
}

#[cfg(windows)]
fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).context("failed to mark the new binary executable")
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Directory where Commitor stores its config state (API key,
/// `last_update_check` marker, ...): `$XDG_CONFIG_HOME/commitor`
/// (defaulting to `~/.config/commitor`) on Unix, `%APPDATA%\commitor`
/// on Windows.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(base).join("commitor"));
    }

    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Ok(PathBuf::from(xdg).join("commitor"));
            }
        }
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config").join("commitor"))
    }
}

/// True when enough time has passed since the last successful update
/// check (or when no marker exists yet).
pub fn update_check_due() -> Result<bool> {
    let path = config_dir()?.join("last_update_check");
    let modified = match fs::metadata(&path) {
        Ok(meta) => meta.modified().context("failed to read timestamp")?,
        Err(_) => return Ok(true),
    };
    Ok(modified.elapsed().unwrap_or_default() >= Duration::from_secs(UPDATE_CHECK_INTERVAL_SECS))
}

/// Record that an update check just succeeded.
pub fn mark_update_checked() -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).context("failed to create the config directory")?;
    fs::File::create(dir.join("last_update_check"))
        .context("failed to record the update check time")?;
    Ok(())
}

/// Record `version` as a pending update that must be installed via
/// `commitor update` before the CLI may be used again.
pub fn record_pending_version(version: &str) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).context("failed to create the config directory")?;
    fs::write(dir.join(PENDING_UPDATE_FILE), version.trim())
        .context("failed to record the pending update")
}

/// The version of a pending update recorded by an earlier update
/// check, if it is still newer than the running binary.
///
/// A stale marker — one that is not newer than the running version,
/// e.g. left behind by a manual reinstall — is treated as absent and
/// cleaned up on the spot.
pub fn pending_version() -> Result<Option<String>> {
    let path = config_dir()?.join(PENDING_UPDATE_FILE);
    let stored = match fs::read_to_string(&path) {
        Ok(stored) => stored.trim().to_string(),
        Err(_) => return Ok(None),
    };

    if stored.is_empty() || !is_newer(&stored, current_version()) {
        let _ = fs::remove_file(&path);
        return Ok(None);
    }

    Ok(Some(stored))
}

/// Clear the pending-update marker, e.g. after a successful
/// self-update.
pub fn clear_pending_version() -> Result<()> {
    let path = config_dir()?.join(PENDING_UPDATE_FILE);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("failed to clear the pending-update marker"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_newer ─────────────────────────────────────────────────────

    #[test]
    fn newer_patch_is_newer() {
        assert!(is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn newer_minor_is_newer() {
        assert!(is_newer("0.2.0", "0.1.9"));
    }

    #[test]
    fn newer_major_is_newer() {
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn equal_versions_are_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn older_latest_is_not_newer() {
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn v_prefix_is_stripped_from_both_sides() {
        assert!(is_newer("v0.2.0", "v0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn unparseable_versions_never_count_as_newer() {
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("v0.2.0", ""));
        assert!(!is_newer("", "v0.1.0"));
    }

    // ── find_matching_asset ──────────────────────────────────────────

    fn release_with_assets(names: &[&str]) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: "v0.2.0".into(),
            html_url: "https://github.com/Commitor-AI/commitor/releases/tag/v0.2.0".into(),
            assets: names
                .iter()
                .map(|name| ReleaseAsset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.com/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn finds_the_asset_for_this_platform() {
        let release = release_with_assets(&[
            "commitor-x86_64-apple-darwin",
            "commitor-aarch64-apple-darwin",
            "commitor-x86_64-pc-windows-msvc.exe",
            "commitor-x86_64-unknown-linux-gnu",
            "source.tar.gz",
        ]);

        let asset = find_matching_asset(&release).unwrap();
        assert_eq!(asset.name, expected_asset_name());
        assert!(asset.name.starts_with("commitor-"));
    }

    #[test]
    fn missing_platform_asset_is_an_error() {
        let release = release_with_assets(&["source.tar.gz"]);
        assert!(find_matching_asset(&release).is_err());
    }

    #[test]
    fn expected_asset_name_follows_release_convention() {
        let name = expected_asset_name();
        assert!(name.starts_with("commitor-"));
        if cfg!(windows) {
            assert!(name.ends_with(".exe"));
            assert!(name.contains("pc-windows-msvc"));
        } else if cfg!(target_os = "macos") {
            assert!(name.ends_with("-apple-darwin"));
        } else if cfg!(target_os = "linux") {
            assert!(name.ends_with("-unknown-linux-gnu"));
        }
    }

    // ── daily-check bookkeeping ──────────────────────────────────────

    #[test]
    fn missing_marker_means_check_is_due() {
        // Point at an empty temp dir via XDG override; HOME fallback
        // also resolves inside it.
        let dir = std::env::temp_dir().join(format!("commitor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        assert!(update_check_due().unwrap());
        mark_update_checked().unwrap();
        assert!(!update_check_due().unwrap());

        // ── pending-update marker ────────────────────────────────────
        assert_eq!(pending_version().unwrap(), None);

        record_pending_version("99.0.0").unwrap();
        assert_eq!(pending_version().unwrap().as_deref(), Some("99.0.0"));

        // A version that is not newer than the running binary is a
        // stale marker: pending_version() drops it.
        record_pending_version("0.0.1").unwrap();
        assert_eq!(pending_version().unwrap(), None);

        // A successful update clears the marker.
        record_pending_version("99.0.0").unwrap();
        clear_pending_version().unwrap();
        assert_eq!(pending_version().unwrap(), None);
        // Clearing twice is fine.
        clear_pending_version().unwrap();

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        std::fs::remove_dir_all(dir).unwrap();
    }
}
