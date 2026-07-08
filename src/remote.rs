// HTTP remote control server for Phosphor.
//
// Serves a single-page web UI and REST API on a configurable port.
// Runs in a background thread — all communication with the iced App
// goes through shared state (Arc<Mutex>) and a command channel.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::Serialize;

use crate::published_playlists::Manifest;

// ─────────────────────────────────────────────────────────────────────────────
//  Commands sent from HTTP server → App (polled on Tick)
// ─────────────────────────────────────────────────────────────────────────────

pub enum RemoteCmd {
    PlayTrack(usize),
    Stop,
    TogglePause,
    NextTrack,
    PrevTrack,
    SetSubtune(u16),
    // Playback QOL — added to bring the remote API up to parity with
    // the library / QOL features that landed in the desktop UI over the
    // last release cycle. Every variant is dispatched 1:1 to an existing
    // `Message::…` handler in `App::poll_remote_commands()`.
    ToggleFavorite(usize),
    ToggleFavoriteCurrent,
    ToggleShuffle,
    CycleRepeat,
    SetSleepTimer(Option<u32>),
    SetVolume(f32),
    Surprise,
    // Library — Published Playlists
    LoadPublishedPlaylist(String),
    RestoreDefaultPlaylist,
    // Library — HVSC browse
    HvscPlay(PathBuf),
    HvscAdd(PathBuf),
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared state: App → HTTP server (updated on every Tick)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Default)]
pub struct RemoteStatus {
    pub state: String,
    pub title: String,
    pub author: String,
    pub released: String,
    pub current_song: u16,
    pub songs: u16,
    pub elapsed_secs: f32,
    pub duration_secs: Option<f32>,
    pub current_index: Option<usize>,
    pub num_sids: usize,
    pub sid_type: String,
    pub is_pal: bool,
    pub engine: String,
    // ── QOL additions (populated in `App::update_remote_state`) ────
    /// Whether the currently-playing track is hearted.
    pub is_favorite: bool,
    /// Host-side master volume in `[0.0, 1.0]`.
    pub master_volume: f32,
    /// Playlist-level playback flags.
    pub shuffle: bool,
    /// "off" | "one" | "all" — string so the web UI can render directly.
    pub repeat: String,
    /// Sleep timer: `Some(mins)` = armed for that many minutes total.
    pub sleep_selected_mins: Option<u32>,
    /// Seconds until the sleep timer fires. `None` = disarmed.
    pub sleep_remaining_secs: Option<u32>,
    /// HVSC sync currently running (drives the "syncing…" indicator).
    pub hvsc_sync_active: bool,
    /// `[files_done, files_total]` when a sync is running; `None` otherwise.
    pub hvsc_sync_progress: Option<[u32; 2]>,
    /// When the app is playing a Published Playlist in read-only mode,
    /// this is the source filename (e.g. `HVSC_Favorite_Top_100.m3u`);
    /// otherwise `None`.
    pub active_published_playlist: Option<String>,
    /// Monotonic playlist-snapshot version. Bumped whenever the
    /// server's cached playlist changes (tracks added/removed, favs
    /// flipped). The web UI mirrors this on the client side and, when
    /// it sees a new value, re-fetches `/api/playlist` to refresh
    /// itself. Without this the browser only knew about the playlist
    /// state it saw at page load.
    pub playlist_version: u64,
}

#[derive(Clone, Serialize)]
pub struct RemotePlaylistEntry {
    pub index: usize,
    pub title: String,
    pub author: String,
    pub duration: Option<u32>,
    pub num_sids: usize,
    pub is_rsid: bool,
    /// True when this entry's md5 is in the favourites DB.
    pub is_favorite: bool,
}

#[derive(Default)]
pub struct SharedRemoteState {
    pub status: RemoteStatus,
    pub playlist: Vec<RemotePlaylistEntry>,
    pub playlist_version: u64,
    /// Snapshot of `config.hvsc_root` — needed by the HVSC-browse
    /// endpoints so the HTTP thread can walk the on-disk tree without
    /// touching App state.
    pub hvsc_root: Option<PathBuf>,
    /// Snapshot of the loaded Published Playlists manifest so the
    /// GET /api/library/playlists endpoint can return it directly.
    pub published_manifest: Option<Manifest>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Server
// ─────────────────────────────────────────────────────────────────────────────

pub fn start_server(port: u16, state: Arc<Mutex<SharedRemoteState>>, cmd_tx: Sender<RemoteCmd>) {
    thread::Builder::new()
        .name("phosphor-http".into())
        .spawn(move || {
            let addr = format!("0.0.0.0:{}", port);
            let server = match tiny_http::Server::http(&addr) {
                Ok(s) => {
                    eprintln!("[phosphor] Remote control: http://localhost:{}", port);
                    s
                }
                Err(e) => {
                    eprintln!("[phosphor] Failed to start HTTP server on {}: {e}", addr);
                    return;
                }
            };

            // Liveness heartbeat state. Prints every ~60 s so the
            // server log confirms the thread is still alive even in
            // long idle stretches.
            let mut requests_handled: u64 = 0;
            let mut last_heartbeat = Instant::now();

            for mut request in server.incoming_requests() {
                if last_heartbeat.elapsed() >= Duration::from_secs(60) {
                    eprintln!(
                        "[remote] server alive: {requests_handled} requests handled"
                    );
                    last_heartbeat = Instant::now();
                }
                requests_handled += 1;
                let url = request.url().to_string();
                let method = request.method().to_string();

                // Wrap the whole match in `catch_unwind` so a panic in
                // ONE handler doesn't kill the server thread. Previous
                // behaviour: any panic (e.g. via a poisoned mutex from
                // the encoder or SID engines) tore down the whole
                // TcpListener, giving every subsequent request
                // ERR_CONNECTION_REFUSED.
                //
                // `AssertUnwindSafe` is needed because `Request` isn't
                // `UnwindSafe`. It's sound here: on panic the request
                // is dropped (tiny_http closes the socket), so no
                // half-mutated shared state escapes.
                let method_s = method.clone();
                let url_s = url.clone();
                let handler = std::panic::AssertUnwindSafe(|| {
                match (method.as_str(), url.as_str()) {
                    // ── Web UI ───────────────────────────────────────────
                    ("GET", "/") | ("GET", "/index.html") => {
                        let resp = tiny_http::Response::from_string(WEB_UI).with_header(
                            "Content-Type: text/html; charset=utf-8"
                                .parse::<tiny_http::Header>()
                                .unwrap(),
                        );
                        let _ = request.respond(resp);
                    }

                    // ── API: audio stream (MP3 to <audio> tag) ───────────
                    // Opens a subscriber and hands it to tiny_http as
                    // the response body. The subscriber implements
                    // `Read`, so tiny_http drives an infinite chunked
                    // response until the browser closes the connection.
                    ("GET", p) if p == "/api/stream.mp3" || p.starts_with("/api/stream.mp3?") => {
                        // Move the stream handling to its OWN thread —
                        // tiny_http's `incoming_requests()` is a single
                        // consumer, so serving the infinite stream body
                        // inline blocks every other request. Safari
                        // routinely opens 2+ parallel probes on an
                        // <audio> URL and the second probe stalls
                        // waiting for the first to close, which looks
                        // exactly like "server dead" from the outside.
                        //
                        // Panic guard: this thread is its own panic
                        // domain — a panic here can't kill the main
                        // server thread.
                        let range_header = request
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("Range"))
                            .map(|h| h.value.as_str().to_string());
                        eprintln!(
                            "[remote] /api/stream.mp3 subscribe (range={})",
                            range_header.as_deref().unwrap_or("none"),
                        );
                        let handled = std::thread::Builder::new()
                            .name("phosphor-stream".into())
                            .spawn(move || {
                                let reader = crate::audio_stream::subscribe();
                                let mut headers: Vec<tiny_http::Header> = Vec::new();
                                headers.push(
                                    "Content-Type: audio/mpeg"
                                        .parse::<tiny_http::Header>()
                                        .unwrap(),
                                );
                                headers.push(
                                    "Cache-Control: no-cache, no-store, must-revalidate"
                                        .parse::<tiny_http::Header>()
                                        .unwrap(),
                                );
                                headers.push(
                                    "Pragma: no-cache"
                                        .parse::<tiny_http::Header>()
                                        .unwrap(),
                                );
                                headers.push(
                                    "Accept-Ranges: none"
                                        .parse::<tiny_http::Header>()
                                        .unwrap(),
                                );
                                headers.push(
                                    "Access-Control-Allow-Origin: *"
                                        .parse::<tiny_http::Header>()
                                        .unwrap(),
                                );
                                let resp = tiny_http::Response::new(
                                    tiny_http::StatusCode(200),
                                    headers,
                                    reader,
                                    None,
                                    None,
                                );
                                let _ = request.respond(resp);
                                eprintln!("[remote] /api/stream.mp3 disconnect");
                            });
                        if let Err(e) = handled {
                            eprintln!("[remote] stream thread spawn failed: {e}");
                        }
                    }

                    // ── API: stream availability ────────────────────────
                    // Web UI polls this to gate the 🔊 button — reports
                    // whether audio has flowed recently enough that a
                    // subscribe would produce audible output.
                    ("GET", "/api/stream/status") => {
                        let json = format!(
                            r#"{{"available":{}}}"#,
                            crate::audio_stream::is_available()
                        );
                        respond_json(request, &json);
                    }

                    // ── API: status ──────────────────────────────────────
                    ("GET", "/api/status") => {
                        let json = {
                            let s = state.lock().unwrap();
                            serde_json::to_string(&s.status).unwrap_or_default()
                        };
                        respond_json(request, &json);
                    }

                    // ── API: playlist (paginated, server-side search) ────
                    // /api/playlist?q=search&offset=0&limit=100
                    ("GET", p) if p.starts_with("/api/playlist") => {
                        let query_str = p.split('?').nth(1).unwrap_or("");
                        let mut q = String::new();
                        let mut offset: usize = 0;
                        let mut limit: usize = 100;
                        for part in query_str.split('&') {
                            if let Some(v) = part.strip_prefix("q=") {
                                q = urldecode(v).to_lowercase();
                            } else if let Some(v) = part.strip_prefix("offset=") {
                                offset = v.parse().unwrap_or(0);
                            } else if let Some(v) = part.strip_prefix("limit=") {
                                limit = v.parse().unwrap_or(100).min(500);
                            }
                        }
                        let json = {
                            let s = state.lock().unwrap();
                            let total = s.playlist.len();
                            let (matched, filtered): (usize, Vec<&RemotePlaylistEntry>) =
                                if q.is_empty() {
                                    (
                                        total,
                                        s.playlist.iter().skip(offset).take(limit).collect(),
                                    )
                                } else {
                                    let all_matches: Vec<&RemotePlaylistEntry> = s
                                        .playlist
                                        .iter()
                                        .filter(|e| {
                                            e.title.to_lowercase().contains(&q)
                                                || e.author.to_lowercase().contains(&q)
                                        })
                                        .collect();
                                    let m = all_matches.len();
                                    (
                                        m,
                                        all_matches
                                            .into_iter()
                                            .skip(offset)
                                            .take(limit)
                                            .collect(),
                                    )
                                };
                            let count = filtered.len();
                            format!(
                                r#"{{"total":{},"matched":{},"offset":{},"count":{},"entries":{}}}"#,
                                total,
                                matched,
                                offset,
                                count,
                                serde_json::to_string(&filtered).unwrap_or_default(),
                            )
                        };
                        respond_json(request, &json);
                    }

                    // ── API: transport controls ──────────────────────────
                    ("POST", "/api/pause") => {
                        let _ = cmd_tx.try_send(RemoteCmd::TogglePause);
                        respond_ok(request);
                    }
                    ("POST", "/api/stop") => {
                        let _ = cmd_tx.try_send(RemoteCmd::Stop);
                        respond_ok(request);
                    }
                    ("POST", "/api/next") => {
                        let _ = cmd_tx.try_send(RemoteCmd::NextTrack);
                        respond_ok(request);
                    }
                    ("POST", "/api/prev") => {
                        let _ = cmd_tx.try_send(RemoteCmd::PrevTrack);
                        respond_ok(request);
                    }

                    // ── API: play track by index ─────────────────────────
                    ("POST", p) if p.starts_with("/api/play/") => {
                        if let Some(idx_str) = p.strip_prefix("/api/play/") {
                            if let Ok(idx) = idx_str.parse::<usize>() {
                                let _ = cmd_tx.try_send(RemoteCmd::PlayTrack(idx));
                                respond_ok(request);
                            } else {
                                respond_error(request, 400, "Invalid index");
                            }
                        } else {
                            respond_error(request, 400, "Missing index");
                        }
                    }

                    // ── API: set subtune ─────────────────────────────────
                    ("POST", p) if p.starts_with("/api/subtune/") => {
                        if let Some(n_str) = p.strip_prefix("/api/subtune/") {
                            if let Ok(n) = n_str.parse::<u16>() {
                                let _ = cmd_tx.try_send(RemoteCmd::SetSubtune(n));
                                respond_ok(request);
                            } else {
                                respond_error(request, 400, "Invalid subtune");
                            }
                        } else {
                            respond_error(request, 400, "Missing subtune");
                        }
                    }

                    // ── API: playback QOL ────────────────────────────────
                    ("POST", "/api/favorite/current") => {
                        let _ = cmd_tx.try_send(RemoteCmd::ToggleFavoriteCurrent);
                        respond_ok(request);
                    }
                    ("POST", p) if p.starts_with("/api/favorite/") => {
                        if let Some(idx_str) = p.strip_prefix("/api/favorite/") {
                            match idx_str.parse::<usize>() {
                                Ok(idx) => {
                                    let _ = cmd_tx.try_send(RemoteCmd::ToggleFavorite(idx));
                                    respond_ok(request);
                                }
                                Err(_) => respond_error(request, 400, "Invalid index"),
                            }
                        } else {
                            respond_error(request, 400, "Missing index");
                        }
                    }
                    ("POST", "/api/shuffle") => {
                        let _ = cmd_tx.try_send(RemoteCmd::ToggleShuffle);
                        respond_ok(request);
                    }
                    ("POST", "/api/repeat") => {
                        let _ = cmd_tx.try_send(RemoteCmd::CycleRepeat);
                        respond_ok(request);
                    }
                    ("POST", p) if p.starts_with("/api/sleep/") => {
                        if let Some(m_str) = p.strip_prefix("/api/sleep/") {
                            match m_str.parse::<u32>() {
                                Ok(0) => {
                                    let _ = cmd_tx.try_send(RemoteCmd::SetSleepTimer(None));
                                    respond_ok(request);
                                }
                                Ok(m) => {
                                    let _ = cmd_tx.try_send(RemoteCmd::SetSleepTimer(Some(m)));
                                    respond_ok(request);
                                }
                                Err(_) => respond_error(request, 400, "Invalid minutes"),
                            }
                        } else {
                            respond_error(request, 400, "Missing minutes");
                        }
                    }
                    ("POST", p) if p.starts_with("/api/volume/") => {
                        if let Some(v_str) = p.strip_prefix("/api/volume/") {
                            match v_str.parse::<f32>() {
                                Ok(v) => {
                                    let _ = cmd_tx
                                        .try_send(RemoteCmd::SetVolume(v.clamp(0.0, 1.0)));
                                    respond_ok(request);
                                }
                                Err(_) => respond_error(request, 400, "Invalid volume"),
                            }
                        } else {
                            respond_error(request, 400, "Missing volume");
                        }
                    }
                    ("POST", "/api/surprise") => {
                        let _ = cmd_tx.try_send(RemoteCmd::Surprise);
                        respond_ok(request);
                    }

                    // ── API: Published Playlists ─────────────────────────
                    ("GET", "/api/library/playlists") => {
                        let json = {
                            let s = state.lock().unwrap();
                            match &s.published_manifest {
                                Some(m) => serde_json::to_string(&m.playlists)
                                    .unwrap_or_else(|_| "[]".to_string()),
                                None => "[]".to_string(),
                            }
                        };
                        respond_json(request, &json);
                    }
                    ("POST", "/api/library/playlists/load") => {
                        match read_body(&mut request) {
                            Ok(body) => match extract_json_string(&body, "file") {
                                Some(file) => {
                                    let _ = cmd_tx
                                        .try_send(RemoteCmd::LoadPublishedPlaylist(file));
                                    respond_ok(request);
                                }
                                None => respond_error(request, 400, "Missing 'file'"),
                            },
                            Err(e) => respond_error(request, 400, &e),
                        }
                    }
                    ("POST", "/api/library/playlists/restore") => {
                        let _ = cmd_tx.try_send(RemoteCmd::RestoreDefaultPlaylist);
                        respond_ok(request);
                    }

                    // ── API: HVSC browse ─────────────────────────────────
                    ("GET", p) if p.starts_with("/api/library/hvsc/authors") => {
                        let query_str = p.split('?').nth(1).unwrap_or("");
                        let category = parse_category(query_str)
                            .unwrap_or(crate::hvsc_browser::HvscCategory::Musicians);
                        let hvsc_root = { state.lock().unwrap().hvsc_root.clone() };
                        match hvsc_root {
                            Some(root) => {
                                let json = list_hvsc_authors(&root, category);
                                respond_json(request, &json);
                            }
                            None => respond_error(request, 503, "HVSC root not configured"),
                        }
                    }
                    ("GET", p) if p.starts_with("/api/library/hvsc/tunes") => {
                        let query_str = p.split('?').nth(1).unwrap_or("");
                        let category = parse_category(query_str)
                            .unwrap_or(crate::hvsc_browser::HvscCategory::Musicians);
                        let author = parse_query_value(query_str, "author").unwrap_or_default();
                        let hvsc_root = { state.lock().unwrap().hvsc_root.clone() };
                        match hvsc_root {
                            Some(root) if !author.is_empty() => {
                                let json = list_hvsc_tunes(&root, category, &author);
                                respond_json(request, &json);
                            }
                            Some(_) => respond_error(request, 400, "Missing 'author'"),
                            None => respond_error(request, 503, "HVSC root not configured"),
                        }
                    }
                    ("POST", "/api/library/hvsc/play") => {
                        match read_body(&mut request) {
                            Ok(body) => match extract_json_string(&body, "path") {
                                Some(path) => {
                                    let _ = cmd_tx
                                        .try_send(RemoteCmd::HvscPlay(PathBuf::from(path)));
                                    respond_ok(request);
                                }
                                None => respond_error(request, 400, "Missing 'path'"),
                            },
                            Err(e) => respond_error(request, 400, &e),
                        }
                    }
                    ("POST", "/api/library/hvsc/add") => {
                        match read_body(&mut request) {
                            Ok(body) => match extract_json_string(&body, "path") {
                                Some(path) => {
                                    let _ = cmd_tx
                                        .try_send(RemoteCmd::HvscAdd(PathBuf::from(path)));
                                    respond_ok(request);
                                }
                                None => respond_error(request, 400, "Missing 'path'"),
                            },
                            Err(e) => respond_error(request, 400, &e),
                        }
                    }

                    _ => {
                        respond_error(request, 404, "Not found");
                    }
                }
                });
                if let Err(payload) = std::panic::catch_unwind(handler) {
                    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "<non-string panic payload>".to_string()
                    };
                    eprintln!(
                        "[remote] handler panic on {method_s} {url_s}: {msg}"
                    );
                }
            }
            eprintln!("[remote] server thread exiting (iterator ended)");
        })
        .expect("Failed to spawn HTTP server thread");
}

fn respond_json(request: tiny_http::Request, json: &str) {
    let resp = tiny_http::Response::from_string(json).with_header(
        "Content-Type: application/json"
            .parse::<tiny_http::Header>()
            .unwrap(),
    );
    let _ = request.respond(resp);
}

fn respond_ok(request: tiny_http::Request) {
    respond_json(request, r#"{"ok":true}"#);
}

fn respond_error(request: tiny_http::Request, code: u16, msg: &str) {
    let json = format!(r#"{{"error":"{}"}}"#, msg);
    let resp = tiny_http::Response::from_string(json)
        .with_status_code(code)
        .with_header(
            "Content-Type: application/json"
                .parse::<tiny_http::Header>()
                .unwrap(),
        );
    let _ = request.respond(resp);
}

/// Minimal percent-decoding for query parameter values.
fn urldecode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            }
        } else if c == '+' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers for the library / QOL endpoints
// ─────────────────────────────────────────────────────────────────────────────

/// Read a POST body into a String. Cap at 4 KiB — we only accept small
/// JSON payloads with one or two fields.
fn read_body(request: &mut tiny_http::Request) -> Result<String, String> {
    use std::io::Read;
    const MAX_BODY: usize = 4096;
    let mut buf = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Body read error: {e}"))?;
    String::from_utf8(buf).map_err(|_| "Invalid UTF-8 in body".to_string())
}

/// Poor-man's JSON string extractor. Accepts payloads shaped like
/// `{"key":"value"}` (possibly with whitespace); returns `None` if the
/// key is missing or the value isn't a plain string. Used for the two
/// endpoints that take a single-field body (`file`, `path`).
///
/// Deliberately does NOT depend on `serde_json` here because we don't
/// need real parsing — the caller controls both ends of the protocol.
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    // Look for  "key" : "..."
    let needle = format!("\"{key}\"");
    let start = body.find(&needle)? + needle.len();
    let rest = body[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    // Find the closing unescaped quote.
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '/' => out.push('/'),
                _ => return None,
            },
            _ => out.push(c),
        }
    }
    None
}

/// Parse `key=value` from a `foo=bar&key=xxx&…` query string.
fn parse_query_value(query: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    query
        .split('&')
        .find(|p| p.starts_with(&prefix))
        .map(|p| urldecode(&p[prefix.len()..]))
}

/// Parse `?category=musicians|demos|games` into an `HvscCategory`.
fn parse_category(query: &str) -> Option<crate::hvsc_browser::HvscCategory> {
    let raw = parse_query_value(query, "category")?;
    match raw.to_lowercase().as_str() {
        "musicians" | "music" => Some(crate::hvsc_browser::HvscCategory::Musicians),
        "demos" | "demo" => Some(crate::hvsc_browser::HvscCategory::Demos),
        "games" | "game" => Some(crate::hvsc_browser::HvscCategory::Games),
        _ => None,
    }
}

#[derive(Serialize)]
struct HvscAuthorRow<'a> {
    name: &'a str,
    display_name: &'a str,
    letter: char,
    path: String,
}

/// Build the author list for the requested category directly from the
/// filesystem. Reuses `HvscBrowser::load_authors_if_needed` — the HTTP
/// thread holds its own throwaway browser instance per request, which
/// is fine because it's just a Vec<> and one shallow readdir walk.
fn list_hvsc_authors(
    hvsc_root: &std::path::Path,
    category: crate::hvsc_browser::HvscCategory,
) -> String {
    let mut b = crate::hvsc_browser::HvscBrowser::new(Some(hvsc_root.to_path_buf()));
    b.set_category(category);
    if let Err(e) = b.load_authors_if_needed() {
        return format!(r#"{{"error":"{}"}}"#, e.replace('"', "'"));
    }
    let rows: Vec<HvscAuthorRow<'_>> = b
        .authors()
        .iter()
        .map(|a| HvscAuthorRow {
            name: a.raw_name.as_str(),
            display_name: a.display_name.as_str(),
            letter: a.letter,
            path: a.path.to_string_lossy().into_owned(),
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

#[derive(Serialize)]
struct HvscTuneRow {
    path: String,
    title: String,
    author: String,
    released: String,
    songs: u16,
    selected_song: u16,
    is_rsid: bool,
    num_sids: usize,
    duration_secs: Option<u32>,
    md5: Option<String>,
    has_stil: bool,
}

/// Walk the requested author folder and enumerate every SID/MUS tune.
/// Called from the HTTP thread — takes tens of ms for typical authors.
///
/// Duration lookup is skipped in this first pass (needs an Arc-shared
/// SonglengthDb which we haven't threaded through yet); the mobile UI
/// picks it up from `/api/status` after playback starts.
fn list_hvsc_tunes(
    hvsc_root: &std::path::Path,
    category: crate::hvsc_browser::HvscCategory,
    author_name: &str,
) -> String {
    let mut b = crate::hvsc_browser::HvscBrowser::new(Some(hvsc_root.to_path_buf()));
    b.set_category(category);
    if b.load_authors_if_needed().is_err() {
        return "[]".to_string();
    }
    let idx = match b.authors().iter().position(|a| a.raw_name == author_name) {
        Some(i) => i,
        None => return "[]".to_string(),
    };
    b.select_author(idx, None, None);
    let rows: Vec<HvscTuneRow> = b
        .tunes()
        .iter()
        .map(|t| HvscTuneRow {
            path: t.entry.path.to_string_lossy().into_owned(),
            title: t.entry.title.clone(),
            author: t.entry.author.clone(),
            released: t.entry.released.clone(),
            songs: t.entry.songs,
            selected_song: t.entry.selected_song,
            is_rsid: t.entry.is_rsid,
            num_sids: t.entry.num_sids,
            duration_secs: t.entry.duration_secs,
            md5: t.entry.md5.clone(),
            has_stil: t.has_stil,
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
//  Embedded Web UI
// ─────────────────────────────────────────────────────────────────────────────

const WEB_UI: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Phosphor Remote</title>
<style>
  * { margin:0; padding:0; box-sizing:border-box; }
  body { background:#0e1014; color:#c8ccd0; font-family:-apple-system,system-ui,sans-serif; }
  .header { background:#161920; padding:12px 16px; text-align:center; border-bottom:1px solid #2a2e36; }
  .header h1 { font-size:18px; color:#5cb870; letter-spacing:2px; }
  .now-playing { padding:16px; text-align:center; min-height:90px; }
  .np-title { font-size:20px; font-weight:600; color:#e0e4e8; }
  .np-author { font-size:14px; color:#8090a0; margin-top:4px; }
  .np-info { font-size:12px; color:#506070; margin-top:6px; }
  .progress { height:3px; background:#1a1e26; margin:0 16px; border-radius:2px; }
  .progress-fill { height:100%; background:linear-gradient(90deg,#3a7,#5cb870); border-radius:2px; transition:width 0.5s; }
  .controls { display:flex; justify-content:center; gap:12px; padding:16px; }
  .controls button { width:52px; height:42px; border:1px solid #2a2e36; border-radius:8px;
    background:#1a1e26; color:#c8ccd0; font-size:18px; cursor:pointer; transition:background 0.15s; }
  .controls button:hover { background:#252a34; }
  .controls button:active { background:#3a7; color:#0e1014; }
  .search { padding:8px 16px; }
  .search input { width:100%; padding:8px 12px; border:1px solid #2a2e36; border-radius:6px;
    background:#1a1e26; color:#c8ccd0; font-size:14px; outline:none; }
  .search input:focus { border-color:#3a7; }
  .playlist { overflow-y:auto; max-height:calc(100vh - 320px); }
  .track { display:flex; padding:8px 16px; border-bottom:1px solid #1a1e26; cursor:pointer;
    transition:background 0.1s; align-items:center; gap:10px; }
  .track:hover { background:#1a1e26; }
  .track.active { background:#1a2a20; border-left:3px solid #5cb870; }
  .track .idx { width:30px; font-size:11px; color:#506070; text-align:right; flex-shrink:0; }
  .track .info { flex:1; min-width:0; }
  .track .t-title { font-size:13px; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
  .track .t-author { font-size:11px; color:#607080; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
  .track .dur { font-size:11px; color:#506070; flex-shrink:0; }
  .state-dot { display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; }
  .state-dot.playing { background:#5cb870; box-shadow:0 0 6px #5cb870; }
  .state-dot.paused { background:#c8a030; }
  .state-dot.stopped { background:#505860; }
  .heart { background:none; border:none; color:#607080; font-size:22px; cursor:pointer;
    padding:0 8px; vertical-align:middle; }
  .heart.on { color:#ff5f7a; }
  .track .heart { font-size:16px; padding:2px 8px; }
  .extras { display:flex; align-items:center; justify-content:center; gap:14px;
    padding:8px 16px; font-size:12px; color:#8090a0; flex-wrap:wrap; }
  .extras select, .extras input[type="range"] { background:#1a1e26; color:#c8ccd0;
    border:1px solid #2a2e36; border-radius:4px; padding:4px 6px; font-size:12px; }
  .extras input[type="range"] { padding:0; width:120px; }
  .sleep-countdown { color:#c8a030; font-family:ui-monospace,monospace; font-size:12px; }
  .banner { padding:8px 16px; background:#2a1e0a; border-bottom:1px solid #3a2a10;
    color:#e8b060; font-size:13px; display:flex; align-items:center; justify-content:space-between; gap:8px; }
  .banner button { background:none; border:1px solid #6a4a20; color:#e8b060;
    padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px; }
  .banner button:hover { background:#3a2a10; }
  .lib-toggle { display:block; margin:8px auto 0; background:none;
    border:1px solid #2a2e36; color:#8090a0; padding:6px 14px;
    border-radius:6px; font-size:12px; cursor:pointer; }
  .lib-toggle:hover { background:#1a1e26; color:#5cb870; }
  .lib { border-top:1px solid #2a2e36; border-bottom:1px solid #2a2e36;
    background:#141821; padding:8px 12px; margin-top:8px; }
  .lib-tabs { display:flex; gap:8px; margin-bottom:8px; }
  .lib-tab { flex:1; padding:6px; background:#1a1e26; border:1px solid #2a2e36;
    color:#8090a0; border-radius:4px; cursor:pointer; font-size:12px;
    text-align:center; }
  .lib-tab.active { background:#1c2e22; border-color:#3a7; color:#5cb870; }
  .lib-list { max-height:280px; overflow-y:auto; }
  .lib-row { padding:8px; border-bottom:1px solid #1a1e26; cursor:pointer;
    display:flex; justify-content:space-between; gap:8px; align-items:center; }
  .lib-row:hover { background:#1a1e26; }
  .lib-row .lib-name { flex:1; font-size:13px; color:#c8ccd0; }
  .lib-row .lib-meta { font-size:11px; color:#607080; flex-shrink:0; }
  .lib-back { padding:6px 0; color:#5cb870; cursor:pointer; font-size:12px; }
  .lib-back:hover { text-decoration:underline; }
  .lib-cat { display:flex; gap:6px; margin-bottom:6px; }
  .lib-cat button { flex:1; padding:4px; background:#1a1e26; border:1px solid #2a2e36;
    color:#8090a0; border-radius:4px; cursor:pointer; font-size:11px; }
  .lib-cat button.on { background:#1c2e22; border-color:#3a7; color:#5cb870; }
  .lib-empty { padding:20px; text-align:center; color:#607080; font-size:12px; }
</style>
</head>
<body>

<div class="header"><h1>PHOSPHOR</h1></div>

<div class="banner" id="pub-banner" style="display:none;">
  <span id="pub-banner-text"></span>
  <button onclick="restoreDefault()">Restore my playlist</button>
</div>

<div class="now-playing" id="np">
  <div class="np-title">
    <span id="np-title">—</span>
    <button class="heart" id="np-heart" onclick="toggleFavCurrent()" title="Favourite">&#9825;</button>
  </div>
  <div class="np-author" id="np-author"></div>
  <div class="np-info" id="np-info"></div>
</div>

<div class="progress"><div class="progress-fill" id="prog" style="width:0%"></div></div>

<div class="controls">
  <button onclick="cmd('prev')" title="Previous">&#9198;</button>
  <button onclick="cmd('stop')" title="Stop">&#9209;</button>
  <button onclick="cmd('pause')" title="Play/Pause" id="pp-btn">&#9208;</button>
  <button onclick="cmd('next')" title="Next">&#9197;</button>
  <button onclick="cmd('surprise')" title="Surprise me — random HVSC tune">&#127922;</button>
  <button onclick="toggleListen()" id="listen-btn" title="Listen in browser">&#128266;</button>
</div>

<!-- Hidden <audio> element driven by the 🔊 button.
     - No `preload="none"` — Safari's <audio> pipeline stalls silently
       when you set .src on a preload=none element and call .play():
       the play() promise never resolves, no error event fires, no
       canplay ever comes. `preload="auto"` (default) is what Safari
       expects.
     - No `crossorigin` attr — switches on CORS-attributed media
       semantics; Safari silently refuses without matching CORS on
       every chunk. -->
<audio id="stream-audio"></audio>

<div class="extras">
  <label>&#128554;
    <select id="sleep" onchange="setSleep(this.value)">
      <option value="0">Off</option>
      <option value="15">15 min</option>
      <option value="30">30 min</option>
      <option value="60">60 min</option>
    </select>
    <span class="sleep-countdown" id="sleep-cd"></span>
  </label>
  <label>&#128266;
    <input type="range" id="vol" min="0" max="100" step="1" value="100"
      oninput="setVolume(this.value)">
  </label>
  <button onclick="cmd('shuffle')" id="shuf-btn" title="Shuffle"
    style="background:none;border:1px solid #2a2e36;color:#8090a0;padding:4px 8px;border-radius:4px;cursor:pointer;">&#128256;</button>
  <button onclick="cmd('repeat')" id="rep-btn" title="Repeat"
    style="background:none;border:1px solid #2a2e36;color:#8090a0;padding:4px 8px;border-radius:4px;cursor:pointer;">&#8634; Off</button>
</div>

<button class="lib-toggle" onclick="toggleLibrary()" id="lib-toggle-btn">&#128218; Library</button>

<div class="lib" id="lib" style="display:none;">
  <div class="lib-tabs">
    <div class="lib-tab active" id="lt-pl" onclick="showLibTab('pl')">&#128203; Playlists</div>
    <div class="lib-tab" id="lt-hv" onclick="showLibTab('hv')">&#128194; HVSC</div>
  </div>

  <div id="lib-pl">
    <div class="lib-list" id="lib-pl-list"></div>
  </div>

  <div id="lib-hv" style="display:none;">
    <div class="lib-cat">
      <button class="on" data-cat="musicians" onclick="setHvscCat('musicians')">Musicians</button>
      <button data-cat="demos" onclick="setHvscCat('demos')">Demos</button>
      <button data-cat="games" onclick="setHvscCat('games')">Games</button>
    </div>
    <div id="lib-hv-crumb" class="lib-back" onclick="hvBack()" style="display:none;">&#8592; Back to authors</div>
    <div class="lib-list" id="lib-hv-list"></div>
  </div>
</div>

<div class="search"><input id="q" placeholder="Search playlist..." oninput="onSearch()"></div>
<div id="pl-info" style="padding:2px 16px;font-size:11px;color:#506070;"></div>
<div class="playlist" id="pl"></div>

<script>
let entries=[], status={}, curIdx=null, total=0, loading=false, searchTimer=null;
// Last-seen server playlist snapshot version. Bumped by the server
// whenever tracks are added/removed or favourites flip. We compare on
// every /api/status poll and re-fetch the playlist when it changes so
// desktop-side actions (Surprise Me, drag-add, folder-add) show up on
// the web UI without a manual refresh.
let lastPlaylistVersion=null;
let volDebounce=null;

async function cmd(c){
  await fetch('/api/'+c,{method:'POST'});
  setTimeout(poll,150);
}

// ── Server-side audio → browser <audio> ────────────────────────
// The 🔊 button toggles a hidden <audio> element that streams MP3
// from the built-in encoder. Cache-busts on each start so browsers
// don't try to resume an old (already-closed) stream. Availability
// is polled from /api/stream/status so the button dims for engines
// (USB / U64) that can't be tapped.
let streamOn=false;
const MEDIA_ERR = {
  1: 'MEDIA_ERR_ABORTED (user aborted)',
  2: 'MEDIA_ERR_NETWORK (network problem)',
  3: 'MEDIA_ERR_DECODE (broken bytes)',
  4: 'MEDIA_ERR_SRC_NOT_SUPPORTED (browser rejected the format)',
};
function attachDiag(el){
  el.addEventListener('error',()=>{
    const err=el.error||{};
    const code=err.code||0;
    const name=MEDIA_ERR[code]||'unknown ('+code+')';
    console.error('[stream] audio error:',name,'| src:',el.currentSrc,'| readyState=',el.readyState,'networkState=',el.networkState);
  },{once:true});
  el.addEventListener('loadstart',()=>console.info('[stream] loadstart'),{once:true});
  el.addEventListener('progress',()=>console.info('[stream] progress'),{once:true});
  el.addEventListener('loadedmetadata',()=>console.info('[stream] loadedmetadata'),{once:true});
  el.addEventListener('canplay',()=>console.info('[stream] canplay'),{once:true});
  el.addEventListener('playing',()=>console.info('[stream] playing'),{once:true});
  el.addEventListener('stalled',()=>console.warn('[stream] stalled'),{once:true});
  el.addEventListener('waiting',()=>console.info('[stream] waiting'),{once:true});
}
function toggleListen(){
  const el=document.getElementById('stream-audio');
  const btn=document.getElementById('listen-btn');
  if(streamOn){
    el.pause();
    // Nuke <source> children and src; some browsers keep the previous
    // fetch alive until we do both.
    el.removeAttribute('src');
    while(el.firstChild)el.removeChild(el.firstChild);
    el.load();
    btn.innerHTML='&#128266;';
    btn.title='Listen in browser';
    streamOn=false;
    return;
  }
  attachDiag(el);
  // Safari-preferred loading pattern: <source type="audio/mpeg"> child
  // element with explicit type hint, then load() to kick the fetch.
  // Assigning .src directly works in Chrome but Safari sometimes routes
  // it through its plugin fallback path when .src is on a live stream.
  while(el.firstChild)el.removeChild(el.firstChild);
  const source=document.createElement('source');
  source.type='audio/mpeg';
  source.src='/api/stream.mp3?t='+Date.now();
  el.appendChild(source);
  el.load();
  // Button flips SYNCHRONOUSLY — Safari's play() promise on live
  // streams often never resolves and never rejects, so relying on the
  // promise to update the button leaves it stuck.
  btn.innerHTML='&#128263;';
  btn.title='Stop listening';
  streamOn=true;
  const pp=el.play();
  if(pp&&pp.catch){
    pp.catch(e=>{
      console.error('[stream] play() rejected:',e.name,e.message);
      btn.title='Play blocked: '+e.name+' — check console';
    });
  }
  setupMediaSession();
}

function setupMediaSession(){
  if(!('mediaSession' in navigator)) return;
  navigator.mediaSession.setActionHandler('play',   ()=>cmd('play'));
  navigator.mediaSession.setActionHandler('pause',  ()=>cmd('pause'));
  navigator.mediaSession.setActionHandler('previoustrack', ()=>cmd('prev'));
  navigator.mediaSession.setActionHandler('nexttrack',     ()=>cmd('next'));
  navigator.mediaSession.setActionHandler('stop', ()=>cmd('stop'));
}

async function pollStreamStatus(){
  try{
    const r=await fetch('/api/stream/status');
    const j=await r.json();
    const btn=document.getElementById('listen-btn');
    if(j.available){
      btn.disabled=false;
      btn.style.opacity='1';
      if(!streamOn) btn.title='Listen in browser';
    } else {
      btn.disabled=!streamOn; // keep clickable to allow "stop" if we ARE listening
      btn.style.opacity=streamOn?'1':'0.4';
      if(!streamOn) btn.title='Streaming not available on the current engine';
    }
  }catch(_){}
}
setInterval(pollStreamStatus,5000);
setTimeout(pollStreamStatus,500);

async function toggleFavCurrent(){
  await fetch('/api/favorite/current',{method:'POST'});
  setTimeout(poll,150);
  setTimeout(()=>loadPlaylist(false),200);
}

async function toggleFav(idx,ev){
  ev.stopPropagation();
  await fetch('/api/favorite/'+idx,{method:'POST'});
  setTimeout(()=>loadPlaylist(false),150);
}

async function setSleep(mins){
  await fetch('/api/sleep/'+mins,{method:'POST'});
  setTimeout(poll,150);
}

function setVolume(pct){
  clearTimeout(volDebounce);
  const v=(pct/100).toFixed(2);
  volDebounce=setTimeout(()=>{
    fetch('/api/volume/'+v,{method:'POST'});
  },100);
}

async function restoreDefault(){
  await fetch('/api/library/playlists/restore',{method:'POST'});
  setTimeout(poll,300);
  setTimeout(()=>loadPlaylist(false),500);
}

// ── Library panel ─────────────────────────────────────────────
let libOpen=false, hvscCat='musicians', hvscAuthor=null;

function toggleLibrary(){
  libOpen=!libOpen;
  document.getElementById('lib').style.display=libOpen?'block':'none';
  document.getElementById('lib-toggle-btn').textContent=libOpen?'✕ Close Library':'\u{1F4DA} Library';
  if(libOpen){
    loadLibPlaylists();
  }
}

function showLibTab(which){
  document.getElementById('lt-pl').classList.toggle('active',which==='pl');
  document.getElementById('lt-hv').classList.toggle('active',which==='hv');
  document.getElementById('lib-pl').style.display=which==='pl'?'block':'none';
  document.getElementById('lib-hv').style.display=which==='hv'?'block':'none';
  if(which==='hv'&&!hvscAuthor){loadHvscAuthors();}
}

async function loadLibPlaylists(){
  const el=document.getElementById('lib-pl-list');
  el.innerHTML='<div class="lib-empty">Loading playlists…</div>';
  try{
    const r=await fetch('/api/library/playlists');
    const list=await r.json();
    if(!list||list.length===0){
      el.innerHTML='<div class="lib-empty">No playlists synced yet.<br>Open Phosphor → Library → Playlists → Sync.</div>';
      return;
    }
    el.innerHTML=list.map(p=>{
      const name=esc(p.name||p.file);
      const desc=p.description?'<div style="font-size:11px;color:#607080;margin-top:2px;">'+esc(p.description)+'</div>':'';
      const tracks=p.tracks?p.tracks+' tracks':'';
      return '<div class="lib-row" onclick="loadPub(\''+esc(p.file)+'\')">'+
        '<div><div class="lib-name">'+name+'</div>'+desc+'</div>'+
        '<span class="lib-meta">'+tracks+'</span></div>';
    }).join('');
  }catch(e){
    el.innerHTML='<div class="lib-empty">Failed to load: '+esc(e.message||'error')+'</div>';
  }
}

async function loadPub(file){
  const body=JSON.stringify({file:file});
  await fetch('/api/library/playlists/load',{method:'POST',
    headers:{'Content-Type':'application/json'},body:body});
  toggleLibrary();
  setTimeout(poll,400);
  setTimeout(()=>loadPlaylist(false),600);
}

function setHvscCat(cat){
  hvscCat=cat;
  hvscAuthor=null;
  document.querySelectorAll('.lib-cat button').forEach(b=>{
    b.classList.toggle('on',b.dataset.cat===cat);
  });
  loadHvscAuthors();
}

async function loadHvscAuthors(){
  hvscAuthor=null;
  document.getElementById('lib-hv-crumb').style.display='none';
  const el=document.getElementById('lib-hv-list');
  el.innerHTML='<div class="lib-empty">Loading authors…</div>';
  try{
    const r=await fetch('/api/library/hvsc/authors?category='+hvscCat);
    const list=await r.json();
    if(list.error){
      el.innerHTML='<div class="lib-empty">'+esc(list.error)+'</div>';
      return;
    }
    if(!list||list.length===0){
      el.innerHTML='<div class="lib-empty">No authors found. Is HVSC synced?</div>';
      return;
    }
    el.innerHTML=list.map(a=>{
      return '<div class="lib-row" onclick="loadHvscTunes(\''+esc(a.name).replace(/\'/g,"&#39;")+'\')">'+
        '<div class="lib-name">'+esc(a.display_name||a.name)+'</div>'+
        '<span class="lib-meta">'+esc(String(a.letter||''))+'</span></div>';
    }).join('');
  }catch(e){
    el.innerHTML='<div class="lib-empty">Failed to load</div>';
  }
}

async function loadHvscTunes(author){
  hvscAuthor=author;
  document.getElementById('lib-hv-crumb').style.display='block';
  document.getElementById('lib-hv-crumb').textContent='← '+author.replace(/_/g,' ');
  const el=document.getElementById('lib-hv-list');
  el.innerHTML='<div class="lib-empty">Loading tunes…</div>';
  try{
    const r=await fetch('/api/library/hvsc/tunes?category='+hvscCat+
      '&author='+encodeURIComponent(author));
    const list=await r.json();
    if(!list||list.length===0){
      el.innerHTML='<div class="lib-empty">No tunes.</div>';
      return;
    }
    el.innerHTML=list.map(t=>{
      const dur=t.duration_secs?fmtTime(t.duration_secs):'';
      const meta=(t.songs>1?'['+t.selected_song+'/'+t.songs+'] ':'')+
        (t.is_rsid?'RSID':'PSID')+(dur?' · '+dur:'');
      const p=esc(t.path).replace(/\'/g,"&#39;");
      return '<div class="lib-row" onclick="playHvsc(\''+p+'\')">'+
        '<div><div class="lib-name">'+esc(t.title)+'</div>'+
        '<div style="font-size:11px;color:#607080;">'+meta+'</div></div>'+
        '<button onclick="addHvsc(\''+p+'\',event)" style="background:none;border:1px solid #2a2e36;color:#5cb870;padding:4px 8px;border-radius:4px;cursor:pointer;font-size:11px;">+</button>'+
        '</div>';
    }).join('');
  }catch(e){
    el.innerHTML='<div class="lib-empty">Failed to load</div>';
  }
}

function hvBack(){
  loadHvscAuthors();
}

async function playHvsc(path){
  const body=JSON.stringify({path:path});
  await fetch('/api/library/hvsc/play',{method:'POST',
    headers:{'Content-Type':'application/json'},body:body});
  toggleLibrary();
  setTimeout(poll,400);
  setTimeout(()=>loadPlaylist(false),600);
}

async function addHvsc(path,ev){
  ev.stopPropagation();
  const body=JSON.stringify({path:path});
  await fetch('/api/library/hvsc/add',{method:'POST',
    headers:{'Content-Type':'application/json'},body:body});
  setTimeout(()=>loadPlaylist(false),200);
}

async function poll(){
  try{
    const r=await fetch('/api/status');
    status=await r.json();
    document.getElementById('np-title').textContent=status.title||'—';
    document.getElementById('np-author').textContent=status.author||'';
    const elapsed=fmtTime(status.elapsed_secs||0);
    const dur=status.duration_secs?fmtTime(status.duration_secs):'--:--';
    const parts=[elapsed+' / '+dur];
    if(status.sid_type)parts.push(status.sid_type);
    if(status.is_pal!==undefined)parts.push(status.is_pal?'PAL':'NTSC');
    if(status.engine)parts.push(status.engine);
    document.getElementById('np-info').innerHTML=
      '<span class="state-dot '+status.state+'"></span>'+parts.join(' \u00b7 ');
    const pct=(status.duration_secs&&status.duration_secs>0)?
      Math.min(100,(status.elapsed_secs/status.duration_secs)*100):0;
    document.getElementById('prog').style.width=pct+'%';
    document.getElementById('pp-btn').innerHTML=
      status.state==='playing'?'\u23F8':'\u25B6';
    if(status.current_index!==curIdx){curIdx=status.current_index;highlightCurrent();}
    // Auto-refresh playlist when the desktop side changes it
    // (Surprise Me, drag-add, folder import, favourite toggle).
    if(status.playlist_version!==undefined){
      if(lastPlaylistVersion===null){
        lastPlaylistVersion=status.playlist_version;
      } else if(status.playlist_version!==lastPlaylistVersion){
        lastPlaylistVersion=status.playlist_version;
        loadPlaylist(false);
      }
    }
    // Heart button (♥/♡) reflects current-track favourite state
    const heart=document.getElementById('np-heart');
    if(heart){
      heart.innerHTML=status.is_favorite?'♥':'♡';
      heart.classList.toggle('on',!!status.is_favorite);
    }
    // Shuffle + repeat toggle button state
    const shufBtn=document.getElementById('shuf-btn');
    if(shufBtn){
      shufBtn.style.color=status.shuffle?'#5cb870':'#8090a0';
      shufBtn.style.borderColor=status.shuffle?'#3a7':'#2a2e36';
    }
    const repBtn=document.getElementById('rep-btn');
    if(repBtn){
      const rep=status.repeat||'off';
      repBtn.innerHTML='↻ '+rep.charAt(0).toUpperCase()+rep.slice(1);
      repBtn.style.color=rep!=='off'?'#5cb870':'#8090a0';
      repBtn.style.borderColor=rep!=='off'?'#3a7':'#2a2e36';
    }
    // Volume slider (only sync if not being edited)
    const vol=document.getElementById('vol');
    if(vol&&document.activeElement!==vol&&status.master_volume!==undefined){
      vol.value=Math.round(status.master_volume*100);
    }
    // Sleep timer state
    const sleepSel=document.getElementById('sleep');
    if(sleepSel&&document.activeElement!==sleepSel){
      sleepSel.value=String(status.sleep_selected_mins||0);
    }
    const cd=document.getElementById('sleep-cd');
    if(cd){
      if(status.sleep_remaining_secs!==null&&status.sleep_remaining_secs!==undefined){
        cd.textContent=' '+fmtTime(status.sleep_remaining_secs);
      }else{
        cd.textContent='';
      }
    }
    // Active-published banner
    const banner=document.getElementById('pub-banner');
    if(banner){
      if(status.active_published_playlist){
        banner.style.display='flex';
        const name=status.active_published_playlist.replace(/\.m3u$/,'').replace(/_/g,' ');
        document.getElementById('pub-banner-text').textContent=
          '\u{1F4CC} Playing published: '+name;
      }else{
        banner.style.display='none';
      }
    }
  }catch(e){}
}

function fmtTime(s){s=Math.floor(s);return Math.floor(s/60)+':'+(s%60<10?'0':'')+s%60;}
function esc(s){return s?s.replace(/&/g,'&amp;').replace(/</g,'&lt;'):'';}

async function loadPlaylist(append){
  if(loading)return;
  loading=true;
  const q=encodeURIComponent(document.getElementById('q').value);
  const offset=append?entries.length:0;
  try{
    const r=await fetch('/api/playlist?q='+q+'&offset='+offset+'&limit=100');
    const data=await r.json();
    total=data.matched;
    if(append){entries=entries.concat(data.entries);}
    else{entries=data.entries;}
    renderList(data.total);
  }catch(e){}
  loading=false;
}

function onSearch(){
  clearTimeout(searchTimer);
  searchTimer=setTimeout(()=>loadPlaylist(false),200);
}

function renderList(totalAll){
  const el=document.getElementById('pl');
  const info=document.getElementById('pl-info');
  const q=document.getElementById('q').value;
  if(q){info.textContent=total+' matches (showing '+entries.length+') of '+totalAll+' total';}
  else{info.textContent=entries.length+' of '+total+' tracks';}
  el.innerHTML=entries.map(t=>{
    const active=t.index===curIdx?'active':'';
    const dur=t.duration?fmtTime(t.duration):'';
    const favClass=t.is_favorite?'heart on':'heart';
    const favGlyph=t.is_favorite?'♥':'♡';
    return '<div class="track '+active+'" onclick="cmd(\'play/'+t.index+'\')">'+
      '<span class="idx">'+(t.index+1)+'</span>'+
      '<div class="info"><div class="t-title">'+esc(t.title)+'</div>'+
      '<div class="t-author">'+esc(t.author)+'</div></div>'+
      '<span class="dur">'+dur+'</span>'+
      '<button class="'+favClass+'" onclick="toggleFav('+t.index+',event)">'+favGlyph+'</button>'+
      '</div>';
  }).join('');
  // Show "load more" if there are more results
  if(entries.length<total){
    el.innerHTML+='<div class="track" onclick="loadPlaylist(true)" '+
      'style="justify-content:center;color:#5cb870;font-size:13px;">'+
      '\u25bc Load more ('+entries.length+'/'+total+')</div>';
  }
}

function highlightCurrent(){
  document.querySelectorAll('.track').forEach(el=>{
    const idx=parseInt(el.querySelector('.idx')?.textContent)-1;
    el.classList.toggle('active',idx===curIdx);
  });
  const active=document.querySelector('.track.active');
  if(active)active.scrollIntoView({block:'nearest',behavior:'smooth'});
}

// Auto-load more when scrolling near bottom
document.getElementById('pl').addEventListener('scroll',function(){
  if(this.scrollTop+this.clientHeight>=this.scrollHeight-50){
    if(entries.length<total)loadPlaylist(true);
  }
});

setInterval(poll,1000);
poll();
loadPlaylist(false);
</script>
</body>
</html>
"##;
