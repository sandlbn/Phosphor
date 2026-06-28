// Assembly64 HTTP client — minimum-viable subset of
// ultimate64-manager's assembly64.rs, just enough to search for releases,
// list their files, and download a single file's bytes.
//
// API base: https://hackerswithstyle.se/leet
// Required header on every request: `client-id: u64manager`
//   (the server validates this against a whitelist — sending any other
//    value, including the project name, returns HTTP 464.)
//
// We only implement the SID-browse path. No preset/category/compotype
// caches, no metadata enrichment — those are nice-to-have features
// deferred to v2.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;

const BASE_URL: &str = "https://hackerswithstyle.se/leet";
const CLIENT_ID_HEADER: &str = "client-id";
const CLIENT_ID_VALUE: &str = "u64manager";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// One search-result entry. Fields are a subset of Assembly64's wire
/// shape — we keep what the UI shows and the playback flow needs.
#[derive(Debug, Clone, Deserialize)]
pub struct AsmEntry {
    /// Item id. STRING in the wire format, e.g. "260349".
    pub id: String,
    pub name: String,
    /// Numeric category id (used together with `id` to fetch files).
    pub category: u32,
    #[serde(default)]
    pub group: String,
    /// Composer / poster handle. Often missing on releases without a
    /// single author (compilations, etc.).
    #[serde(default)]
    pub handle: String,
    /// Release year. 0 means "unknown".
    #[serde(default)]
    pub year: u32,
    /// Free-form release date, e.g. "1986-01-01" or empty.
    #[serde(default)]
    pub released: String,
    /// Rating in [0, 10]; 0 means no community rating yet.
    #[serde(default)]
    pub rating: u32,
    /// Last-updated timestamp on Assembly64 (display-only).
    #[serde(default)]
    pub updated: String,
}

/// One file inside an entry's `contentEntry` array.
#[derive(Debug, Clone, Deserialize)]
pub struct AsmFile {
    pub id: u32,
    pub path: String,
    #[serde(default)]
    pub size: u64,
}

impl AsmFile {
    pub fn is_sid(&self) -> bool {
        let lower = self.path.to_ascii_lowercase();
        lower.ends_with(".sid") || lower.ends_with(".psid") || lower.ends_with(".rsid")
    }
}

#[derive(Deserialize)]
struct ContentEntryResponse {
    #[serde(rename = "contentEntry", default)]
    content_entry: Vec<AsmFile>,
}

#[derive(Debug)]
pub enum AssemblyError {
    /// AQL syntax rejected by the server (HTTP 463 or 464).
    AqlSyntax(String),
    /// Any other HTTP status outside the 2xx range.
    Http(StatusCode),
    /// Network / TLS / connect failure.
    Network(String),
    /// JSON deserialisation failed (server returned unexpected shape).
    Json(String),
}

impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AqlSyntax(msg) => write!(f, "AQL syntax error: {msg}"),
            Self::Http(s) => write!(f, "HTTP {s}"),
            Self::Network(e) => write!(f, "Network: {e}"),
            Self::Json(e) => write!(f, "JSON: {e}"),
        }
    }
}

impl std::error::Error for AssemblyError {}

/// Clonable so `Task::perform` futures can hold an owned copy. The inner
/// `reqwest::Client` is already Arc-backed, so this is cheap.
#[derive(Debug, Clone)]
pub struct Assembly64Client {
    http: Client,
}

impl Default for Assembly64Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembly64Client {
    pub fn new() -> Self {
        let builder = Client::builder()
            .user_agent("phosphor-assembly64/0.4")
            .timeout(SEARCH_TIMEOUT);
        let http = crate::config::apply_proxy(builder)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    /// Run an AQL query. `offset` + `limit` are positional path segments
    /// (not query params). `limit` is typically 50.
    pub async fn search(
        &self,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<AsmEntry>, AssemblyError> {
        let url = format!(
            "{}/search/aql/{}/{}?query={}",
            BASE_URL,
            offset,
            limit,
            encode_aql(query)
        );
        let resp = self
            .http
            .get(&url)
            .header(CLIENT_ID_HEADER, CLIENT_ID_VALUE)
            .send()
            .await
            .map_err(|e| AssemblyError::Network(e.to_string()))?;

        let status = resp.status();
        if status == StatusCode::from_u16(463).unwrap()
            || status == StatusCode::from_u16(464).unwrap()
        {
            // Server returns a small JSON like {"errorCode":463,"timestamp":...}
            // We don't bother parsing the body — the status itself is the signal.
            return Err(AssemblyError::AqlSyntax(format!(
                "Query rejected by server (HTTP {})",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(AssemblyError::Http(status));
        }
        resp.json::<Vec<AsmEntry>>()
            .await
            .map_err(|e| AssemblyError::Json(e.to_string()))
    }

    /// List the files contained in one entry. Used to expand a search
    /// hit inline; the UI then filters to `.sid` paths.
    pub async fn list_files(
        &self,
        item_id: &str,
        category_id: u32,
    ) -> Result<Vec<AsmFile>, AssemblyError> {
        let url = format!("{}/search/entries/{}/{}", BASE_URL, item_id, category_id);
        let resp = self
            .http
            .get(&url)
            .header(CLIENT_ID_HEADER, CLIENT_ID_VALUE)
            .send()
            .await
            .map_err(|e| AssemblyError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AssemblyError::Http(status));
        }
        let body = resp
            .json::<ContentEntryResponse>()
            .await
            .map_err(|e| AssemblyError::Json(e.to_string()))?;
        Ok(body.content_entry)
    }

    /// Download one file's raw bytes. Uses a longer timeout than the
    /// API calls because some files (D64s, CRTs) are larger.
    pub async fn download(
        &self,
        item_id: &str,
        category_id: u32,
        file_id: u32,
    ) -> Result<Vec<u8>, AssemblyError> {
        let url = format!(
            "{}/search/bin/{}/{}/{}",
            BASE_URL, item_id, category_id, file_id
        );
        // Build a per-call client so we can use a longer timeout without
        // affecting the shared search timeout.
        let dl_builder = Client::builder()
            .user_agent("phosphor-assembly64/0.4")
            .timeout(DOWNLOAD_TIMEOUT);
        let dl = crate::config::apply_proxy(dl_builder)
            .build()
            .map_err(|e| AssemblyError::Network(e.to_string()))?;
        let resp = dl
            .get(&url)
            .header(CLIENT_ID_HEADER, CLIENT_ID_VALUE)
            .send()
            .await
            .map_err(|e| AssemblyError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AssemblyError::Http(status));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AssemblyError::Network(e.to_string()))?;
        Ok(bytes.to_vec())
    }
}

/// Percent-encode AQL for the `?query=` parameter.
///
/// Ported from ultimate64-manager. Escape space, `"`, `#`, `&`, `+`, `%`,
/// `<`, `=`, `>`, control bytes, and any non-ASCII byte. **Leave `:` and
/// `*` alone** so AQL stays readable on the wire (`name:"commando"` etc.).
pub fn encode_aql(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        let needs_escape = b <= 0x20
            || b >= 0x7F
            || matches!(b, b'"' | b'#' | b'&' | b'+' | b'%' | b'<' | b'=' | b'>');
        if needs_escape {
            out.push(b'%');
            out.push(HEX[((b >> 4) & 0xF) as usize]);
            out.push(HEX[(b & 0xF) as usize]);
        } else {
            out.push(b);
        }
    }
    // Safe — every byte we emit is ASCII.
    String::from_utf8(out).expect("ASCII only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_aql_preserves_colon_and_star() {
        assert_eq!(encode_aql("name:commando"), "name:commando");
        assert_eq!(encode_aql("name:*elite*"), "name:*elite*");
    }

    #[test]
    fn encode_aql_escapes_spaces_and_quotes() {
        assert_eq!(encode_aql(r#"name:"two words""#), "name:%22two%20words%22");
    }

    #[test]
    fn encode_aql_escapes_ampersand_and_plus() {
        assert_eq!(encode_aql("a&b+c%d"), "a%26b%2Bc%25d");
    }

    #[test]
    fn encode_aql_preserves_sort_order() {
        // The default baseline query.
        assert_eq!(
            encode_aql("sort:updated order:desc"),
            "sort:updated%20order:desc"
        );
    }

    #[test]
    fn asmfile_is_sid_extension_check() {
        assert!(AsmFile {
            id: 0,
            path: "Commando.sid".into(),
            size: 0
        }
        .is_sid());
        assert!(AsmFile {
            id: 0,
            path: "DEEP/PATH/foo.SID".into(),
            size: 0
        }
        .is_sid());
        assert!(!AsmFile {
            id: 0,
            path: "commando.d64".into(),
            size: 0
        }
        .is_sid());
        assert!(!AsmFile {
            id: 0,
            path: "commando.prg".into(),
            size: 0
        }
        .is_sid());
    }
}
