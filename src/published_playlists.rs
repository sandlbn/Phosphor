// Published playlists HTTP client.
//
// Fetches a small manifest from the Phosphor GitHub repo (served via the
// raw.githubusercontent.com CDN — no auth, no rate limit) and downloads
// individual M3U files on demand. Cached on disk under
// `<config_dir>/published_playlists/`.
//
// Manifest schema (see `playlists/index.json` in the repo):
//   { "version": 1, "playlists": [ { "file", "name", "description",
//                                    "tracks", "sha256" }, ... ] }

use std::path::PathBuf;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

const RAW_BASE_URL: &str = "https://raw.githubusercontent.com/sandlbn/Phosphor/main/playlists";
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
pub struct PublishedPlaylistMeta {
    pub file: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tracks: u32,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub playlists: Vec<PublishedPlaylistMeta>,
}

/// Cheap to clone — the inner `Client` is Arc-backed.
#[derive(Debug, Clone)]
pub struct PublishedPlaylistsClient {
    http: Client,
}

impl Default for PublishedPlaylistsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PublishedPlaylistsClient {
    pub fn new() -> Self {
        let builder = Client::builder()
            .user_agent("phosphor-published-playlists/0.4")
            .timeout(FETCH_TIMEOUT);
        let http = crate::config::apply_proxy(builder)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    pub async fn fetch_index(&self) -> Result<Manifest, String> {
        let url = format!("{}/index.json", RAW_BASE_URL);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Network: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let body = resp.text().await.map_err(|e| format!("Read body: {e}"))?;
        serde_json::from_str::<Manifest>(&body).map_err(|e| format!("JSON: {e}"))
    }

    /// Download one M3U into the cache directory and return its on-disk path.
    /// Overwrites unconditionally so a delta-sync re-fetch picks up edits.
    pub async fn download_playlist(
        &self,
        file: &str,
        cache_dir: PathBuf,
    ) -> Result<PathBuf, String> {
        let url = format!("{}/{}", RAW_BASE_URL, urlencode_filename(file));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Network: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let body = resp.bytes().await.map_err(|e| format!("Read body: {e}"))?;
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .map_err(|e| format!("Create cache dir: {e}"))?;
        let target = cache_dir.join(file);
        tokio::fs::write(&target, &body)
            .await
            .map_err(|e| format!("Write {}: {e}", target.display()))?;
        Ok(target)
    }
}

/// Percent-encode just the bytes that aren't safe in a path segment.
/// Filenames in the manifest are produced by our own import script —
/// ASCII alphanumeric, `_`, `-`, `.`, `(`, `)` — but we still defend
/// against the rare case of a name with a space or unicode character.
fn urlencode_filename(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(name.len());
    for &b in name.as_bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'(' | b')');
        if safe {
            out.push(b);
        } else {
            out.push(b'%');
            out.push(HEX[((b >> 4) & 0xF) as usize]);
            out.push(HEX[(b & 0xF) as usize]);
        }
    }
    String::from_utf8(out).expect("ASCII only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_through_safe_filename() {
        assert_eq!(
            urlencode_filename("HVSC_Favorite_Top_100.m3u"),
            "HVSC_Favorite_Top_100.m3u"
        );
        assert_eq!(
            urlencode_filename("Zyron___Music_Collection__01__1990_.m3u"),
            "Zyron___Music_Collection__01__1990_.m3u"
        );
    }

    #[test]
    fn urlencode_escapes_spaces() {
        assert_eq!(urlencode_filename("two words.m3u"), "two%20words.m3u");
    }

    #[test]
    fn manifest_deserialises_minimal_shape() {
        let json = r#"{
            "version": 1,
            "playlists": [
                { "file": "a.m3u", "name": "A", "tracks": 3, "sha256": "abc" }
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.playlists.len(), 1);
        assert_eq!(m.playlists[0].file, "a.m3u");
        assert_eq!(m.playlists[0].description, "");
        assert_eq!(m.playlists[0].sha256, "abc");
    }
}
