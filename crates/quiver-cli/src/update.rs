// SPDX-License-Identifier: AGPL-3.0-only
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const REPO: &str = "achref-soua/quiver";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn platform() -> Result<&'static str> {
    match std::env::consts::OS {
        "linux" => Ok("linux"),
        "macos" => Ok("macos"),
        other => bail!(
            "quiver update is not supported on {other}; \
             download manually from https://github.com/{REPO}/releases"
        ),
    }
}

fn arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        other => bail!("unsupported architecture: {other}"),
    }
}

/// Query the GitHub Releases API for the latest tag (e.g. `"0.17.0"`, strip `v`).
fn fetch_latest_version(agent: &ureq::Agent) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp: serde_json::Value = agent
        .get(&url)
        .set("User-Agent", &format!("quiver-cli/{CURRENT_VERSION}"))
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("failed to reach GitHub API")?
        .into_json()
        .context("failed to parse GitHub API response")?;

    resp["tag_name"]
        .as_str()
        .map(|s| s.trim_start_matches('v').to_owned())
        .context("GitHub API response missing tag_name")
}

/// Download bytes from `url`.
fn fetch_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut body = agent
        .get(url)
        .set("User-Agent", &format!("quiver-cli/{CURRENT_VERSION}"))
        .call()
        .with_context(|| format!("failed to download {url}"))?
        .into_reader();
    let mut buf = Vec::new();
    body.read_to_end(&mut buf)
        .with_context(|| format!("failed to read response from {url}"))?;
    Ok(buf)
}

/// Returns true if `latest` is a strictly higher semver than `current`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

fn parse_semver(v: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = v
        .trim_start_matches('v')
        .splitn(3, '.')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

/// Verify SHA-256 of `data` against a checksum string (hex, optionally `"hash  filename"` format).
pub fn verify_sha256(data: &[u8], checksum_line: &str) -> Result<()> {
    let expected = checksum_line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(data);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!("checksum mismatch — expected {expected}, got {actual}; aborting");
    }
    Ok(())
}

/// Atomically replace `current_exe` with `new_binary`.
fn atomic_replace(current_exe: &PathBuf, new_binary: &[u8]) -> Result<()> {
    let parent = current_exe
        .parent()
        .context("cannot determine parent directory of the current binary")?;

    let tmp = parent.join(format!(".quiver-update-{}.tmp", std::process::id()));
    fs::write(&tmp, new_binary).context("failed to write updated binary to temp file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
    }

    fs::rename(&tmp, current_exe).with_context(|| {
        format!(
            "failed to replace {} — is it write-protected?",
            current_exe.display()
        )
    })?;

    Ok(())
}

fn run_blocking(check_only: bool) -> Result<()> {
    let agent = ureq::AgentBuilder::new().build();

    print!("Checking for updates... ");
    let latest = fetch_latest_version(&agent)?;
    println!("latest: v{latest}  installed: v{CURRENT_VERSION}");

    if !is_newer(CURRENT_VERSION, &latest) {
        println!("Quiver v{CURRENT_VERSION} is already up to date.");
        return Ok(());
    }

    println!("New version available: v{latest}");

    if check_only {
        println!(
            "Run `quiver update` (without --check) to install, \
             or visit https://github.com/{REPO}/releases/tag/v{latest}"
        );
        return Ok(());
    }

    let os = platform()?;
    let arch = arch()?;
    let asset_name = format!("quiver-{os}-{arch}");
    let base_url = format!("https://github.com/{REPO}/releases/download/v{latest}/{asset_name}");
    let checksum_url = format!("{base_url}.sha256");

    println!("Downloading {asset_name}...");
    let binary = fetch_bytes(&agent, &base_url)?;

    println!("Downloading checksum...");
    let checksum_bytes = fetch_bytes(&agent, &checksum_url)?;
    let checksum_str =
        String::from_utf8(checksum_bytes).context("checksum file is not valid UTF-8")?;

    println!("Verifying SHA-256 checksum...");
    verify_sha256(&binary, &checksum_str)?;
    println!("Checksum OK.");

    let current_exe = env::current_exe().context("cannot determine current binary path")?;
    println!("Installing to {}...", current_exe.display());
    atomic_replace(&current_exe, &binary)?;
    println!("Quiver updated to v{latest}. Run `quiver --version` to confirm.");

    Ok(())
}

/// Entry point for `quiver update`.
pub async fn run(check_only: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || run_blocking(check_only))
        .await
        .context("update task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_newer() {
        assert!(is_newer("0.16.0", "0.17.0"));
        assert!(is_newer("0.17.0", "1.0.0"));
        assert!(is_newer("0.9.9", "0.10.0"));
    }

    #[test]
    fn semver_not_newer() {
        assert!(!is_newer("0.17.0", "0.17.0"));
        assert!(!is_newer("0.17.0", "0.16.9"));
        assert!(!is_newer("1.0.0", "0.99.9"));
    }

    #[test]
    fn sha256_correct_hash_passes() {
        let data = b"hello quiver";
        let mut h = Sha256::new();
        h.update(data);
        let hash = format!("{:x}", h.finalize());
        verify_sha256(data, &hash).expect("correct hash should pass");
    }

    #[test]
    fn sha256_wrong_hash_fails() {
        let data = b"hello quiver";
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(data, wrong).is_err());
    }

    #[test]
    fn sha256_checksum_file_format() {
        // checksum files often look like "abc123  filename"
        let data = b"quiver binary content";
        let mut h = Sha256::new();
        h.update(data);
        let hash = format!("{:x}", h.finalize());
        let line = format!("{hash}  quiver-linux-x86_64");
        verify_sha256(data, &line).expect("should parse hash from checksum file format");
    }
}
