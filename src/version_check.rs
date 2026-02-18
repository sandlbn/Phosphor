use serde::Deserialize;

/// GitHub repo to check for updates.
const GITHUB_REPO: &str = "sandlbn/Phosphor";

/// GitHub release asset info.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// GitHub release info.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

/// Version check result shown in the UI.
#[derive(Debug, Clone)]
pub struct NewVersionInfo {
    pub version: String,
    /// Direct download URL for the platform-specific binary, or release page.
    pub download_url: String,
}

/// Find the download URL for the current platform from release assets.
fn find_platform_asset(assets: &[GitHubAsset]) -> Option<&str> {
    let target = if cfg!(target_os = "windows") {
        ".exe"
    } else if cfg!(target_os = "macos") {
        ".dmg"
    } else if cfg!(target_os = "linux") {
        ".AppImage"
    } else {
        return None;
    };

    assets
        .iter()
        .find(|a| a.name.ends_with(target))
        .map(|a| a.browser_download_url.as_str())
}

/// Fetch the latest release from GitHub and compare with current version.
pub async fn check_github_release(current_version: &str) -> Result<Option<NewVersionInfo>, String> {
    let client = reqwest::Client::builder()
        .user_agent("Phosphor-SID-Player")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Client error: {e}"))?;

    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("GitHub API error: {}", response.status()));
    }

    let release: GitHubRelease = response
        .json()
        .await
        .map_err(|e| format!("Parse error: {e}"))?;

    // Remove 'v' prefix if present for comparison
    let latest = release.tag_name.trim_start_matches('v');
    let current = current_version.trim_start_matches('v');

    if is_newer_version(latest, current) {
        let download_url = find_platform_asset(&release.assets)
            .map(|s| s.to_string())
            .unwrap_or(release.html_url);

        Ok(Some(NewVersionInfo {
            version: release.tag_name.clone(),
            download_url,
        }))
    } else {
        Ok(None)
    }
}

/// Compare semantic versions (e.g., "0.3.4" > "0.3.3").
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };

    let lp = parse(latest);
    let cp = parse(current);

    for i in 0..lp.len().max(cp.len()) {
        let l = lp.get(i).copied().unwrap_or(0);
        let c = cp.get(i).copied().unwrap_or(0);
        if l > c {
            return true;
        } else if l < c {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_comparison() {
        assert!(is_newer_version("0.3.4", "0.3.3"));
        assert!(is_newer_version("0.4.0", "0.3.9"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(!is_newer_version("0.3.3", "0.3.3"));
        assert!(!is_newer_version("0.3.2", "0.3.3"));
    }
}
