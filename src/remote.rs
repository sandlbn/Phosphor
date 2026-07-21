// HTTP remote control server for Phosphor.
//
// Serves a single-page web UI and REST API on a configurable port.
// Runs in a background thread — all communication with the iced App
// goes through shared state (Arc<Mutex>) and a command channel.

use std::io::Read as _;
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
    /// Load all liked tracks as the fresh playlist. Server-side calls
    /// through the same `Message::LoadFavoritesPlaylist` handler the
    /// desktop uses so the resolve/heal path is identical.
    LoadFavoritesPlaylist,
    // ── Playlist editing ─────────────────────────────────────────
    /// Remove the track at the given playlist index. Mirrors the
    /// desktop's `ContextMenuRemove` handler.
    PlaylistRemove(usize),
    /// Clear the entire playlist. Mirrors `ClearPlaylist`.
    PlaylistClear,
    /// Move the track at `from` to position `to`. Web UI uses this
    /// for drag-to-reorder and the up/down context-menu items.
    /// Desktop's `MoveToTop` is a special case (to=0).
    PlaylistMove {
        from: usize,
        to: usize,
    },
    /// Sort the playlist by the given column name. `col` matches the
    /// desktop `SortColumn` labels: "title" | "author" | "released"
    /// | "duration" | "type" | "sids".
    PlaylistSort(String),
    /// Import M3U/PLS content into the current playlist. Body is the
    /// file text. `is_pls` is a hint (default false = m3u).
    PlaylistImport {
        m3u: String,
        is_pls: bool,
    },
    // ── Subtune navigation ───────────────────────────────────────
    SubtuneNext,
    SubtunePrev,
    // ── HVSC sync ─────────────────────────────────────────────────
    HvscSyncStart,
    HvscSyncCancel,
    // ── Quick settings from the web ──────────────────────────────
    /// Toggle `config.skip_rsid` — auto-skip RSID tunes.
    ToggleSkipRsid,
    /// Toggle `config.force_stereo_2sid` — mirror SID1 to both channels.
    ToggleForceStereo2sid,
    /// Set `config.surprise_source` — "hvsc" or "playlist".
    SetSurpriseSource(String),
    /// Direct-set repeat mode, bypassing the cycle order.
    /// Accepted values: "off", "one", "all".
    SetRepeatMode(String),
    /// Direct-set shuffle, bypassing the toggle.
    SetShuffle(bool),
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
    /// Live mirror of `config.skip_rsid` so the web UI can render its
    /// state on the settings panel.
    #[serde(default)]
    pub skip_rsid: bool,
    /// Live mirror of `config.force_stereo_2sid`.
    #[serde(default)]
    pub force_stereo_2sid: bool,
    /// Live mirror of `config.surprise_source` — "hvsc" or "playlist".
    #[serde(default)]
    pub surprise_source: String,
}

/// A single row served by `GET /api/recent`. Mirrors `RecentEntry` but
/// without the raw filesystem path leaking into JSON (skip-serialized).
/// The web UI hits `POST /api/library/hvsc/play` with the same path to
/// replay a track, so we keep it as a `String` server-side.
#[derive(Clone, Serialize)]
pub struct RemoteRecentEntry {
    pub title: String,
    pub author: String,
    pub released: String,
    /// Human-readable relative timestamp ("2 h ago", "just now").
    pub played_at_relative: String,
    #[serde(skip_serializing)]
    pub path: PathBuf,
    /// Zero-based index the browser sends back with the play command
    /// so we don't need the path on the wire.
    pub index: usize,
}

/// A single row served by `GET /api/favorites`. Same shape as the
/// Recent snapshot — title + author + duration + a per-row index
/// the browser sends back to `POST /api/favorites/play/{idx}`.
/// Path stays server-side (skip-serialized) so we don't leak the
/// filesystem layout to a curious client.
#[derive(Clone, Serialize)]
pub struct RemoteFavouriteEntry {
    pub title: String,
    pub author: String,
    pub released: String,
    pub duration_secs: Option<u32>,
    #[serde(skip_serializing)]
    pub path: Option<PathBuf>,
    pub index: usize,
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
    /// Absolute file path on the server. Included so the M3U export
    /// endpoint (`GET /api/playlist/export.m3u`) can produce a real
    /// portable playlist — without paths the exported file couldn't
    /// be replayed anywhere else. `#[serde(skip_serializing)]` keeps
    /// paths off the JSON we send to the browser: a curious client
    /// could otherwise read the server's filesystem layout.
    #[serde(skip_serializing)]
    pub path: PathBuf,
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
    /// Live mirror of `config.http_stream_enabled`. When false the
    /// `/api/stream.mp3` endpoint returns 503 without touching the
    /// encoder, so a user who never turns streaming on pays zero CPU
    /// for it regardless of what the web UI does.
    pub stream_enabled: bool,
    /// Snapshot of the recently-played history, ordered newest-first.
    /// Refreshed in `update_remote_state`; served over `GET /api/recent`.
    pub recently_played: Vec<RemoteRecentEntry>,
    /// Snapshot of the persisted Liked collection, newest-first (same
    /// order as `FavoritesDb::entries`). Served over `GET /api/favorites`.
    pub liked: Vec<RemoteFavouriteEntry>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Server
// ─────────────────────────────────────────────────────────────────────────────

/// Attempt to bind. On Windows a `TcpListener` doesn't release its
/// port immediately after `Server::unblock()` + `Arc<Server>` drop —
/// Winsock's close path takes a beat, and a rapid Stop→Start from the
/// Settings toggle otherwise fails with `Address already in use` and
/// looks like the toggle is broken. Retry with backoff (~1.5 s total)
/// covers every case I've reproduced. macOS + Linux release
/// immediately so they get a single attempt with no wait.
#[cfg(target_os = "windows")]
fn try_bind(addr: &str) -> Result<tiny_http::Server, String> {
    let mut delay = Duration::from_millis(100);
    let mut last_err = String::new();
    for attempt in 0..5u32 {
        match tiny_http::Server::http(addr) {
            Ok(s) => {
                if attempt > 0 {
                    eprintln!("[phosphor] Remote bound after {attempt} retry attempt(s)");
                }
                return Ok(s);
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt < 4 {
                    eprintln!(
                        "[phosphor] Port not yet released (attempt {}) — retrying in {} ms",
                        attempt + 1,
                        delay.as_millis(),
                    );
                    thread::sleep(delay);
                    delay = delay.saturating_mul(2);
                }
            }
        }
    }
    Err(last_err)
}

#[cfg(not(target_os = "windows"))]
fn try_bind(addr: &str) -> Result<tiny_http::Server, String> {
    tiny_http::Server::http(addr).map_err(|e| e.to_string())
}

/// Spawn the HTTP server thread and return an `Arc<Server>` handle
/// that lets the caller shut it down cleanly via `Server::unblock()`.
///
/// Returns `None` if the socket bind fails (address in use, permission
/// denied, etc.) — the caller keeps `http_remote_running = false` and
/// surfaces the error to the user via the eprintln we emit below.
pub fn start_server(
    port: u16,
    state: Arc<Mutex<SharedRemoteState>>,
    cmd_tx: Sender<RemoteCmd>,
) -> Option<Arc<tiny_http::Server>> {
    let addr = format!("0.0.0.0:{}", port);
    let server = match try_bind(&addr) {
        Ok(s) => {
            eprintln!("[phosphor] Remote control: http://localhost:{}", port);
            Arc::new(s)
        }
        Err(e) => {
            eprintln!("[phosphor] Failed to start HTTP server on {}: {e}", addr);
            return None;
        }
    };
    let server_for_thread = server.clone();
    thread::Builder::new()
        .name("phosphor-http".into())
        .spawn(move || {
            let server = server_for_thread;
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
                        // Gate on config.http_stream_enabled. When the
                        // user hasn't opted in, return 503 so the URL
                        // exists (avoids 404 confusion) but no encoder
                        // spins up — saves ~15% CPU when the feature
                        // isn't in use.
                        let enabled = state
                            .lock()
                            .map(|s| s.stream_enabled)
                            .unwrap_or(false);
                        if !enabled {
                            eprintln!("[remote] /api/stream.mp3 refused — disabled in Settings");
                            respond_error(request, 503, "Audio streaming disabled — enable in Settings → Network");
                            return;
                        }
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
                        let enabled = state
                            .lock()
                            .map(|s| s.stream_enabled)
                            .unwrap_or(false);
                        let available = enabled && crate::audio_stream::is_available();
                        let json = format!(
                            r#"{{"available":{available},"enabled":{enabled}}}"#
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
                    // Absolute: `/api/subtune/{n}`  Relative: `next` / `prev`
                    ("POST", "/api/subtune/next") => {
                        let _ = cmd_tx.try_send(RemoteCmd::SubtuneNext);
                        respond_ok(request);
                    }
                    ("POST", "/api/subtune/prev") => {
                        let _ = cmd_tx.try_send(RemoteCmd::SubtunePrev);
                        respond_ok(request);
                    }
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

                    // ── API: playlist editing ────────────────────────────
                    ("POST", "/api/playlist/clear") => {
                        let _ = cmd_tx.try_send(RemoteCmd::PlaylistClear);
                        respond_ok(request);
                    }
                    ("POST", p) if p.starts_with("/api/playlist/remove/") => {
                        match p
                            .strip_prefix("/api/playlist/remove/")
                            .and_then(|s| s.parse::<usize>().ok())
                        {
                            Some(idx) => {
                                let _ = cmd_tx.try_send(RemoteCmd::PlaylistRemove(idx));
                                respond_ok(request);
                            }
                            None => respond_error(request, 400, "Invalid index"),
                        }
                    }
                    ("POST", "/api/playlist/move") => {
                        // JSON body: {"from": <int>, "to": <int>}
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let parsed: Option<(usize, usize)> = (|| {
                            let v: serde_json::Value = serde_json::from_str(&body).ok()?;
                            Some((
                                v.get("from")?.as_u64()? as usize,
                                v.get("to")?.as_u64()? as usize,
                            ))
                        })();
                        match parsed {
                            Some((from, to)) => {
                                let _ = cmd_tx.try_send(RemoteCmd::PlaylistMove { from, to });
                                respond_ok(request);
                            }
                            None => respond_error(request, 400, "Body must be {from, to}"),
                        }
                    }
                    ("POST", p) if p.starts_with("/api/playlist/sort/") => {
                        let col = p
                            .strip_prefix("/api/playlist/sort/")
                            .unwrap_or("")
                            .to_string();
                        if col.is_empty() {
                            respond_error(request, 400, "Missing column");
                        } else {
                            let _ = cmd_tx.try_send(RemoteCmd::PlaylistSort(col));
                            respond_ok(request);
                        }
                    }
                    ("GET", "/api/playlist/export.m3u") => {
                        let m3u = {
                            let s = state.lock().unwrap();
                            let mut out = String::from("#EXTM3U\n");
                            for e in &s.playlist {
                                let dur = e.duration.unwrap_or(0) as i64;
                                let display = if !e.author.is_empty() {
                                    format!("{} - {}", e.author, e.title)
                                } else {
                                    e.title.clone()
                                };
                                out.push_str(&format!("#EXTINF:{dur},{display}\n"));
                                out.push_str(&format!("{}\n", e.path.display()));
                            }
                            out
                        };
                        let resp = tiny_http::Response::from_string(m3u)
                            .with_header(
                                "Content-Type: audio/x-mpegurl"
                                    .parse::<tiny_http::Header>()
                                    .unwrap(),
                            )
                            .with_header(
                                "Content-Disposition: attachment; filename=\"phosphor-playlist.m3u\""
                                    .parse::<tiny_http::Header>()
                                    .unwrap(),
                            );
                        let _ = request.respond(resp);
                    }
                    ("POST", "/api/playlist/import") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        if body.is_empty() {
                            respond_error(request, 400, "Empty body");
                        } else {
                            let is_pls = body.contains("[playlist]")
                                || body.contains("File1=");
                            let _ = cmd_tx.try_send(RemoteCmd::PlaylistImport {
                                m3u: body,
                                is_pls,
                            });
                            respond_ok(request);
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
                    // Load all liked tracks as a fresh playlist. Fires
                    // the same `Message::LoadFavoritesPlaylist` path
                    // the desktop UI uses, so resolve/heal semantics
                    // are identical.
                    ("POST", "/api/favorites/play") => {
                        let _ = cmd_tx.try_send(RemoteCmd::LoadFavoritesPlaylist);
                        respond_ok(request);
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

                    // ── API: playlist preview (published) ────────────────
                    // Reads the cached M3U from disk and returns its
                    // parsed track list so the web UI can render the
                    // same accordion preview the desktop uses. Cache
                    // files land under `<config>/published_playlists`
                    // — we get the path via `state.published_manifest`
                    // → filename lookup rather than trusting an
                    // arbitrary user-supplied path.
                    ("GET", p) if p.starts_with("/api/library/playlists/preview/") => {
                        let file_raw = p
                            .strip_prefix("/api/library/playlists/preview/")
                            .unwrap_or("");
                        let file = urldecode(file_raw);
                        // Only allow files listed in the manifest —
                        // this keeps the endpoint from doubling as a
                        // "read any file in the cache dir" oracle.
                        let known = state
                            .lock()
                            .unwrap()
                            .published_manifest
                            .as_ref()
                            .map(|m| m.playlists.iter().any(|pl| pl.file == file))
                            .unwrap_or(false);
                        if !known {
                            respond_error(request, 404, "Not in manifest");
                        } else {
                            match crate::published_playlists::cache_dir() {
                                Some(dir) => {
                                    let path = dir.join(&file);
                                    match std::fs::read_to_string(&path) {
                                        Ok(content) => {
                                            let tracks =
                                                crate::playlist::parse_m3u_preview(&content);
                                            let json: Vec<serde_json::Value> = tracks
                                                .iter()
                                                .map(|t| {
                                                    serde_json::json!({
                                                        "title": t.title,
                                                        "author": t.author,
                                                        "duration_secs": t.duration_secs,
                                                    })
                                                })
                                                .collect();
                                            respond_json(
                                                request,
                                                &serde_json::to_string(&json).unwrap_or_default(),
                                            );
                                        }
                                        Err(e) => respond_error(
                                            request,
                                            404,
                                            &format!("Not cached: {e}"),
                                        ),
                                    }
                                }
                                None => respond_error(request, 500, "No cache dir"),
                            }
                        }
                    }

                    // ── API: quick settings ──────────────────────────────
                    // Small toggles from the web UI's settings panel.
                    // All map 1:1 to existing desktop Message handlers
                    // so the config side-effects (save to disk,
                    // apply-to-audio-thread, refresh dependent UI) are
                    // identical.
                    ("POST", "/api/settings/skip-rsid") => {
                        let _ = cmd_tx.try_send(RemoteCmd::ToggleSkipRsid);
                        respond_ok(request);
                    }
                    ("POST", "/api/settings/force-stereo") => {
                        let _ = cmd_tx.try_send(RemoteCmd::ToggleForceStereo2sid);
                        respond_ok(request);
                    }
                    ("POST", p) if p.starts_with("/api/settings/surprise-source/") => {
                        let src = p
                            .strip_prefix("/api/settings/surprise-source/")
                            .unwrap_or("")
                            .to_string();
                        if src == "hvsc" || src == "playlist" {
                            let _ = cmd_tx.try_send(RemoteCmd::SetSurpriseSource(src));
                            respond_ok(request);
                        } else {
                            respond_error(request, 400, "hvsc or playlist");
                        }
                    }
                    // Direct-set variants of shuffle / repeat so
                    // scripts and quick-toggle buttons don't have to
                    // guess the current state before flipping.
                    ("POST", p) if p.starts_with("/api/repeat/") => {
                        let m = p
                            .strip_prefix("/api/repeat/")
                            .unwrap_or("")
                            .to_string();
                        if matches!(m.as_str(), "off" | "one" | "all") {
                            let _ = cmd_tx.try_send(RemoteCmd::SetRepeatMode(m));
                            respond_ok(request);
                        } else {
                            respond_error(request, 400, "off | one | all");
                        }
                    }
                    ("POST", p) if p.starts_with("/api/shuffle/") => {
                        let v = p.strip_prefix("/api/shuffle/").unwrap_or("");
                        match v {
                            "on" | "true" | "1" => {
                                let _ = cmd_tx.try_send(RemoteCmd::SetShuffle(true));
                                respond_ok(request);
                            }
                            "off" | "false" | "0" => {
                                let _ = cmd_tx.try_send(RemoteCmd::SetShuffle(false));
                                respond_ok(request);
                            }
                            _ => respond_error(request, 400, "on | off"),
                        }
                    }

                    // ── API: HVSC sync trigger ────────────────────────────
                    // Kicks the same rsync the desktop's Settings →
                    // Library → Sync button uses. Progress polls into
                    // `status.hvsc_sync_active` / `hvsc_sync_progress`.
                    ("POST", "/api/library/hvsc/sync/start") => {
                        let _ = cmd_tx.try_send(RemoteCmd::HvscSyncStart);
                        respond_ok(request);
                    }
                    ("POST", "/api/library/hvsc/sync/cancel") => {
                        let _ = cmd_tx.try_send(RemoteCmd::HvscSyncCancel);
                        respond_ok(request);
                    }

                    // ── API: recently played ─────────────────────────────
                    // Returns the last-N snapshot from the Recent DB,
                    // newest first. Click on the web UI hits
                    // `POST /api/recent/play/{idx}` which the server
                    // resolves to a path and dispatches HvscPlay.
                    ("GET", "/api/recent") => {
                        let json = {
                            let s = state.lock().unwrap();
                            serde_json::to_string(&s.recently_played).unwrap_or_default()
                        };
                        respond_json(request, &json);
                    }
                    ("POST", p) if p.starts_with("/api/recent/play/") => {
                        let idx = p
                            .strip_prefix("/api/recent/play/")
                            .and_then(|s| s.parse::<usize>().ok());
                        let path = idx.and_then(|i| {
                            state.lock().unwrap()
                                .recently_played
                                .get(i)
                                .map(|e| e.path.clone())
                        });
                        match path {
                            Some(p) => {
                                let _ = cmd_tx.try_send(RemoteCmd::HvscPlay(p));
                                respond_ok(request);
                            }
                            None => respond_error(request, 404, "Not in recent list"),
                        }
                    }

                    // ── API: Liked collection ────────────────────────────
                    // GET returns the current Liked snapshot. POST plays
                    // just that row (falls back to `HvscPlay(path)`
                    // machinery). Play-all uses the existing
                    // `/api/favorites/play` LoadFavoritesPlaylist path.
                    ("GET", "/api/favorites") => {
                        let json = {
                            let s = state.lock().unwrap();
                            serde_json::to_string(&s.liked).unwrap_or_default()
                        };
                        respond_json(request, &json);
                    }
                    ("POST", p) if p.starts_with("/api/favorites/play/") => {
                        let idx = p
                            .strip_prefix("/api/favorites/play/")
                            .and_then(|s| s.parse::<usize>().ok());
                        let path = idx.and_then(|i| {
                            state
                                .lock()
                                .unwrap()
                                .liked
                                .get(i)
                                .and_then(|e| e.path.clone())
                        });
                        match path {
                            Some(p) => {
                                let _ = cmd_tx.try_send(RemoteCmd::HvscPlay(p));
                                respond_ok(request);
                            }
                            None => respond_error(
                                request,
                                404,
                                "Unresolvable — try Load All so the fallback chain heals paths",
                            ),
                        }
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
            eprintln!(
                "[remote] server thread exiting cleanly after {requests_handled} requests"
            );
        })
        .expect("Failed to spawn HTTP server thread");
    Some(server)
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
  /* Row-level heart: quiet by default so idle rows aren't
     heart-noisy. Filled state stays visible (that's the "already
     liked" indicator); empty state hides until hover. Matches
     Apple Music / Tidal row conventions. */
  .track .heart { font-size:16px; padding:2px 6px;
    background:none; border:none; color:#607080; cursor:pointer;
    opacity:0; transition:opacity 0.12s; }
  .track:hover .heart { opacity:1; }
  .track .heart.on { opacity:1; color:#ff5f7a; }
  @media (hover: none) {
    /* Touch: unhoverable — show the heart at half opacity when
       not liked so the affordance is discoverable. */
    .track .heart { opacity:0.5; }
  }
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
  /* Quick-settings drawer */
  .settings-panel { border-top:1px solid #2a2e36; border-bottom:1px solid #2a2e36;
    background:#141821; padding:10px 14px; margin-top:8px;
    display:flex; flex-direction:column; gap:8px; }
  .settings-row { display:flex; align-items:center; justify-content:space-between;
    gap:12px; }
  .settings-lbl { color:#c8ccd0; font-size:13px; }
  .settings-panel .tb-btn.on { background:#1c2e22; border-color:#3a7; color:#5cb870; }

  /* Liked-tab header (count + Play all) */
  .lib-header { padding:6px 0 8px; border-bottom:1px solid #1a1e26; margin-bottom:6px; }
  .lib-header-row { display:flex; align-items:center; justify-content:space-between; gap:8px; }
  .lib-header-title { font-size:13px; color:#c8ccd0; }
  .lk-row-play { background:none; border:1px solid #2a2e36; color:#8090a0;
    padding:2px 8px; border-radius:4px; cursor:pointer; font-size:12px; }
  .lk-row-play:hover { color:#5cb870; border-color:#3a7; }

  /* Published-playlist preview accordion */
  .pub-preview { background:#0d1218; border-bottom:1px solid #1a1e26;
    padding:8px 10px; font-size:12px; }
  .pub-preview-actions { display:flex; gap:6px; margin-bottom:6px; }
  .pub-preview-body { max-height:220px; overflow-y:auto; }
  .pub-preview-track { display:flex; gap:8px; padding:3px 0; color:#c8ccd0;
    align-items:center; }
  .pub-preview-idx { width:24px; text-align:right; color:#607080; flex-shrink:0; }
  .pub-preview-dur { margin-left:auto; color:#607080; font-family:ui-monospace,monospace;
    font-size:11px; flex-shrink:0; }

  /* ── Playlist toolbar ─────────────────────────────────────────── */
  .pl-toolbar { display:flex; align-items:center; flex-wrap:wrap; gap:6px;
    padding:6px 16px 4px; }
  .tb-btn { background:#1a1e26; border:1px solid #2a2e36; color:#c8ccd0;
    padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;
    text-decoration:none; display:inline-block; user-select:none; }
  .tb-btn:hover { background:#22262f; border-color:#3a4048; color:#ffffff; }
  .tb-btn.active { background:#2a1a20; border-color:#c05070; color:#ff8090; }
  .tb-btn.danger { color:#ff8080; }
  .tb-btn.danger:hover { background:#2a1a1a; border-color:#c05050; }
  .tb-select { background:#1a1e26; border:1px solid #2a2e36; color:#c8ccd0;
    padding:4px 8px; border-radius:4px; font-size:12px; cursor:pointer; }

  /* ── Row hover-action group + drag handle ─────────────────────── */
  .track { position:relative; }
  .track.dragging { opacity:0.4; }
  .track.drag-over { border-top:2px solid #5cb870; }
  .track .actions { display:flex; align-items:center; gap:4px; opacity:0;
    transition:opacity 0.12s; flex-shrink:0; }
  .track:hover .actions { opacity:1; }
  .track .actions button { background:none; border:none; color:#8090a0;
    font-size:16px; padding:2px 6px; cursor:pointer; border-radius:3px; }
  .track .actions button:hover { background:#2a2e36; color:#ffffff; }
  /* On touch devices you can't hover — show actions dimmed by default. */
  @media (hover: none) {
    .track .actions { opacity:0.5; }
  }

  /* ── Right-click / long-press context menu ─────────────────────── */
  #ctx-menu { position:fixed; z-index:1000; background:#1a1e26;
    border:1px solid #2a2e36; border-radius:6px; padding:4px 0;
    min-width:180px; box-shadow:0 6px 18px rgba(0,0,0,0.45); display:none; }
  #ctx-menu .ctx-item { padding:8px 14px; font-size:13px; color:#c8ccd0;
    cursor:pointer; user-select:none; display:flex; align-items:center; gap:8px; }
  #ctx-menu .ctx-item:hover { background:#22262f; color:#ffffff; }
  #ctx-menu .ctx-item.danger { color:#ff8080; }
  #ctx-menu .ctx-item.danger:hover { background:#2a1a1a; }
  #ctx-menu .ctx-sep { height:1px; background:#2a2e36; margin:4px 0; }

  /* ── Toast stack (bottom-centre notifications) ─────────────────── */
  #toast-stack { position:fixed; bottom:16px; left:50%; transform:translateX(-50%);
    display:flex; flex-direction:column-reverse; gap:8px; z-index:900;
    pointer-events:none; }
  .toast { background:#1a2620; border:1px solid #3a5540; color:#c8e0d0;
    padding:8px 14px; border-radius:6px; font-size:13px; box-shadow:0 4px 12px rgba(0,0,0,0.4);
    animation:toast-in 0.18s ease-out; max-width:70vw; pointer-events:auto; }
  .toast.danger { background:#261a1a; border-color:#553a3a; color:#e0c8c8; }
  @keyframes toast-in {
    from { transform:translateY(8px); opacity:0; }
    to { transform:translateY(0); opacity:1; }
  }

  /* ── Subtune stepper ─────────────────────────────────────────── */
  .subtune-stepper { display:inline-flex; align-items:center; gap:4px;
    padding:2px 6px; background:#1a1e26; border:1px solid #2a2e36;
    border-radius:4px; font-size:12px; color:#c8ccd0; user-select:none;
    vertical-align:middle; }
  .subtune-stepper button { background:none; border:none; color:#8090a0;
    padding:2px 6px; cursor:pointer; font-size:11px; }
  .subtune-stepper button:hover { color:#5cb870; }
  #subtune-badge { font-family:ui-monospace,monospace; color:#e0e4e8;
    min-width:34px; text-align:center; }

  /* ── Track-row animations ────────────────────────────────────
     Uses the built-in View Transitions API (Chrome/Edge) — see the
     wrapper around renderList in JS. Browsers without support fall
     back to the plain snap-render. Respects reduced-motion. */
  ::view-transition-old(root),
  ::view-transition-new(root) {
    animation-duration: 220ms;
  }
  @media (prefers-reduced-motion: reduce) {
    ::view-transition-old(root),
    ::view-transition-new(root) { animation: none; }
  }

  /* ── Skeleton loading rows ───────────────────────────────────
     Six ghost rows shown while /api/playlist is fetching. The
     shimmer keyframes give the "still loading" cue without spinner
     jitter — modern-player convention. */
  .skeleton-row { display:flex; padding:8px 16px; border-bottom:1px solid #1a1e26;
    align-items:center; gap:10px; }
  .skeleton-bar { height:12px; border-radius:3px;
    background:linear-gradient(90deg, #1a1e26 0%, #22262f 40%, #1a1e26 80%);
    background-size:200% 100%; animation:shimmer 1.3s linear infinite; }
  .skeleton-idx { width:30px; height:11px; }
  .skeleton-title { flex:1; height:12px; }
  .skeleton-dur { width:40px; height:11px; }
  @keyframes shimmer {
    from { background-position:200% 0; }
    to   { background-position:-200% 0; }
  }

  /* ── Friendly empty state ────────────────────────────────────
     Shown when the playlist has zero tracks. Presents the three
     ways to fill it (Load Liked / Library / Import) so the user
     isn't staring at a blank list. */
  .pl-empty { padding:30px 20px; text-align:center; color:#8090a0; font-size:14px; }
  .pl-empty .em-hint { margin-bottom:14px; color:#c8ccd0; font-size:15px; }
  .pl-empty .em-actions { display:flex; flex-wrap:wrap; justify-content:center; gap:8px; }
  .pl-empty .em-actions button { background:#1a1e26; border:1px solid #2a2e36;
    color:#c8ccd0; padding:8px 14px; border-radius:6px; cursor:pointer; font-size:13px; }
  .pl-empty .em-actions button:hover { background:#22262f; border-color:#3a7; color:#5cb870; }

  /* ── Mobile "Now Playing" bar ─────────────────────────────────
     On narrow viewports the header/now-playing region scrolls off
     when you dig into the playlist. This bar sticks to the bottom
     of the viewport with the essentials — enough to control
     playback without scrolling back up. Desktop keeps the roomier
     top-of-page player. */
  #np-mobile { display:none; }
  @media (max-width: 640px) {
    /* Push the playlist above the bar so the bottom rows aren't
       hidden under it. Height matches the bar (~64 px + safe-area). */
    body { padding-bottom: calc(70px + env(safe-area-inset-bottom)); }
    #np-mobile { display:flex; position:fixed; left:0; right:0; bottom:0;
      z-index:800; background:#0d1218; border-top:1px solid #1a2028;
      padding:8px 12px calc(8px + env(safe-area-inset-bottom));
      align-items:center; gap:10px;
      box-shadow:0 -6px 18px rgba(0,0,0,0.4); }
    #np-mobile .npm-info { flex:1; min-width:0; }
    #np-mobile .npm-title { font-size:13px; color:#e0e4e8;
      white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
    #np-mobile .npm-author { font-size:11px; color:#8090a0;
      white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
    #np-mobile button { background:none; border:none; color:#c8ccd0;
      font-size:20px; padding:4px 8px; cursor:pointer; }
    #np-mobile .npm-prog { position:absolute; top:0; left:0; height:2px;
      background:#5cb870; transition:width 0.4s linear; }
    /* Move the toast stack up so it doesn't hide behind the bar. */
    #toast-stack { bottom: calc(80px + env(safe-area-inset-bottom)); }
  }
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
    <button class="heart" id="np-heart" onclick="toggleFavCurrent()" title="Add to Liked">&#9825;</button>
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
  <span id="subtune-stepper" class="subtune-stepper" style="display:none;">
    <button onclick="cmd('subtune/prev')" title="Previous subtune">&#9664;</button>
    <span id="subtune-badge">1/1</span>
    <button onclick="cmd('subtune/next')" title="Next subtune">&#9654;</button>
  </span>
  <button onclick="cmd('surprise')" title="Surprise me — random HVSC tune">&#127922;</button>
  <button onclick="toggleListen()" id="listen-btn" title="Listen in browser">&#128266;</button>
  <span id="stream-state" style="font-size:11px;color:#8090a0;margin-left:4px;">idle</span>
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
  <label title="Sleep timer — Phosphor stops playback after the chosen delay">&#128554;
    <select id="sleep" onchange="setSleep(this.value)" title="Sleep timer">
      <option value="0">Off</option>
      <option value="15">15 min</option>
      <option value="30">30 min</option>
      <option value="60">60 min</option>
    </select>
    <span class="sleep-countdown" id="sleep-cd"></span>
  </label>
  <label title="Master volume — applies to every playback engine (USB / SIDLite / reSID / U64)">&#128266;
    <input type="range" id="vol" min="0" max="100" step="1" value="100"
      oninput="setVolume(this.value)" title="Master volume 0–100%">
  </label>
  <button onclick="cmd('shuffle')" id="shuf-btn" title="Shuffle — random-order playback of the current playlist"
    style="background:none;border:1px solid #2a2e36;color:#8090a0;padding:4px 8px;border-radius:4px;cursor:pointer;">&#128256;</button>
  <button onclick="cmd('repeat')" id="rep-btn" title="Repeat — cycle through Off → All → One"
    style="background:none;border:1px solid #2a2e36;color:#8090a0;padding:4px 8px;border-radius:4px;cursor:pointer;">&#8634; Off</button>
</div>

<div style="display:flex;gap:6px;justify-content:center;margin-top:8px;">
  <button class="lib-toggle" onclick="toggleLibrary()" id="lib-toggle-btn"
    title="Open the Library panel — Playlists, Liked, HVSC browser, Recent history">&#128218; Library</button>
  <button class="lib-toggle" onclick="toggleSettings()" id="settings-toggle-btn"
    title="Quick settings — Skip RSID, Force stereo, Surprise source, Repeat, Shuffle">&#9881; Settings</button>
</div>

<!-- Quick settings drawer. Mirrors the desktop Settings → General
     tab's toggles so a phone user can flip Skip RSID / Force stereo /
     Surprise source without walking to the desk. -->
<div class="settings-panel" id="settings-panel" style="display:none;">
  <div class="settings-row" title="RSID tunes need a real C64 to run correctly. When enabled, Phosphor auto-skips them and moves to the next PSID.">
    <label class="settings-lbl">Skip RSID tunes</label>
    <button class="tb-btn" id="set-skip-rsid" onclick="postSettings('skip-rsid')"
      title="Toggle: auto-skip RSID tracks">—</button>
  </div>
  <div class="settings-row" title="Mirrors SID1 writes onto SID2's register bank so mono tunes still hit both audio channels.">
    <label class="settings-lbl">Force stereo (2SID mirror)</label>
    <button class="tb-btn" id="set-force-stereo" onclick="postSettings('force-stereo')"
      title="Toggle: mirror SID1 → both channels">—</button>
  </div>
  <div class="settings-row" title="What the 🎲 Surprise button picks from — the whole HVSC tree or just the tracks in your current playlist.">
    <label class="settings-lbl">🎲 Surprise source</label>
    <div style="display:flex;gap:4px;">
      <button class="tb-btn" id="set-surp-hvsc" onclick="setSurpriseSource('hvsc')"
        title="Surprise picks a random tune from the entire HVSC library">HVSC</button>
      <button class="tb-btn" id="set-surp-pl" onclick="setSurpriseSource('playlist')"
        title="Surprise picks a random tune from the currently-loaded playlist">Playlist</button>
    </div>
  </div>
  <div class="settings-row" title="Repeat mode — Off plays the queue once, One loops the current tune, All loops the whole playlist.">
    <label class="settings-lbl">🔁 Repeat</label>
    <div style="display:flex;gap:4px;">
      <button class="tb-btn" data-rep="off" onclick="setRepeat('off')"
        title="Play the queue once then stop">Off</button>
      <button class="tb-btn" data-rep="one" onclick="setRepeat('one')"
        title="Loop the current tune forever">One</button>
      <button class="tb-btn" data-rep="all" onclick="setRepeat('all')"
        title="Loop the whole playlist">All</button>
    </div>
  </div>
  <div class="settings-row" title="Shuffle plays tracks in a random order without repeats until every tune has been played once.">
    <label class="settings-lbl">🔀 Shuffle</label>
    <div style="display:flex;gap:4px;">
      <button class="tb-btn" data-shuf="on" onclick="setShuffle(true)"
        title="Enable shuffle">On</button>
      <button class="tb-btn" data-shuf="off" onclick="setShuffle(false)"
        title="Disable shuffle (play in queue order)">Off</button>
    </div>
  </div>
</div>

<div class="lib" id="lib" style="display:none;">
  <div class="lib-tabs">
    <div class="lib-tab active" id="lt-pl" onclick="showLibTab('pl')">&#128203; Playlists</div>
    <div class="lib-tab" id="lt-lk" onclick="showLibTab('lk')">&#10084; Liked</div>
    <div class="lib-tab" id="lt-hv" onclick="showLibTab('hv')">&#128194; HVSC</div>
    <div class="lib-tab" id="lt-rc" onclick="showLibTab('rc')">&#128276; Recent</div>
  </div>

  <div id="lib-pl">
    <div class="lib-list" id="lib-pl-list"></div>
  </div>

  <div id="lib-lk" style="display:none;">
    <div class="lib-header" id="lib-lk-header"></div>
    <div class="lib-list" id="lib-lk-list"></div>
  </div>

  <div id="lib-rc" style="display:none;">
    <div class="lib-list" id="lib-rc-list"></div>
  </div>

  <div id="lib-hv" style="display:none;">
    <div class="lib-cat">
      <button class="on" data-cat="musicians" onclick="setHvscCat('musicians')">Musicians</button>
      <button data-cat="demos" onclick="setHvscCat('demos')">Demos</button>
      <button data-cat="games" onclick="setHvscCat('games')">Games</button>
    </div>
    <div id="lib-hv-crumb" class="lib-back" onclick="hvBack()" style="display:none;">&#8592; Back to authors</div>
    <div id="hv-sync-row" style="display:flex;gap:6px;align-items:center;padding:6px 0;font-size:12px;color:#8090a0;">
      <button class="tb-btn" id="hv-sync-btn" onclick="toggleHvscSync()"
        title="Trigger an rsync of the High Voltage SID Collection from hvsc.brona.dk">&#8595; Sync HVSC</button>
      <span id="hv-sync-status"></span>
    </div>
    <div class="lib-list" id="lib-hv-list"></div>
  </div>
</div>

<div class="search"><input id="q" placeholder="Search playlist..." oninput="onSearch()"></div>

<!-- Playlist toolbar. Sits between the search box and the playlist so
     every actionable operation on the current playlist has one home.
     Modern-player convention: manage/edit affordances above the list;
     row-level actions inline on hover / long-press. -->
<div class="pl-toolbar" id="pl-toolbar">
  <button class="tb-btn" onclick="pickImportM3U()" title="Import an M3U file into the current playlist">
    &#8593; Import M3U
  </button>
  <a class="tb-btn" href="/api/playlist/export.m3u" download="playlist.m3u" title="Download the current playlist as an M3U file">
    &#8595; Save
  </a>
  <button class="tb-btn" onclick="clearPlaylist()" title="Remove every track from the playlist">
    &#128465; Clear
  </button>
  <button class="tb-btn" id="fav-only-chip" onclick="toggleFavOnly()" title="Show only your liked tracks in this playlist">
    &#9776; Liked only
  </button>
  <select class="tb-select" id="sort-select" onchange="sortPlaylist(this.value)" title="Reorder the playlist by a column">
    <option value="">Sort by…</option>
    <option value="title">Title</option>
    <option value="author">Author</option>
    <option value="released">Released</option>
    <option value="duration">Duration</option>
    <option value="type">Type</option>
    <option value="sids">SID count</option>
    <option value="index">Original order</option>
  </select>
  <input id="import-m3u-input" type="file" accept=".m3u,.m3u8,.pls" style="display:none;" onchange="onImportM3U(event)">
</div>

<div id="pl-info" style="padding:2px 16px;font-size:11px;color:#506070;"></div>
<div class="playlist" id="pl"></div>
<div id="np-mobile">
  <div class="npm-prog" id="npm-prog"></div>
  <div class="npm-info">
    <div class="npm-title" id="npm-title">—</div>
    <div class="npm-author" id="npm-author"></div>
  </div>
  <button onclick="cmd('prev')" title="Previous track">&#9198;</button>
  <button onclick="cmd('pause')" title="Play / Pause" id="npm-pp">&#9208;</button>
  <button onclick="cmd('next')" title="Next track">&#9197;</button>
</div>

<div id="toast-stack"></div>
<div id="ctx-menu" onclick="event.stopPropagation()">
  <div class="ctx-item" onclick="ctxAction('play')"><span>&#9654;</span> Play now</div>
  <div class="ctx-item" onclick="ctxAction('fav')"><span>&#9829;</span> <span id="ctx-fav-label">Toggle Liked</span></div>
  <div class="ctx-sep"></div>
  <div class="ctx-item" onclick="ctxAction('top')"><span>&#8593;</span> Move to top</div>
  <div class="ctx-item" onclick="ctxAction('up')"><span>&#8593;</span> Move up</div>
  <div class="ctx-item" onclick="ctxAction('down')"><span>&#8595;</span> Move down</div>
  <div class="ctx-sep"></div>
  <div class="ctx-item" onclick="ctxAction('copy')"><span>&#128203;</span> Copy title</div>
  <div class="ctx-item danger" onclick="ctxAction('remove')"><span>&#128465;</span> Remove from playlist</div>
</div>

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

// (Legacy `loadLiked` transport-row helper removed — the
// "Load liked as playlist" verb now lives inside the Library panel's
// ❤ Liked tab as `playAllLiked()`, matching Spotify's convention
// that the Liked collection is a destination, not a transport
// control.)

// ── Server-side audio → browser <audio> ────────────────────────
// The 🔊 button toggles a hidden <audio> element that streams MP3
// from the built-in encoder. Cache-busts on each start so browsers
// don't try to resume an old (already-closed) stream. Availability
// is polled from /api/stream/status so the button dims for engines
// (USB / U64) that can't be tapped.
let streamOn=false;
let streamEnabled=false;   // Config toggle from /api/stream/status.
let streamAvailable=false; // Audio flowed recently — informational only.
const MEDIA_ERR = {
  1: 'aborted',
  2: 'network',
  3: 'decode',
  4: 'not supported',
};
function setStreamState(txt, colour){
  const el=document.getElementById('stream-state');
  if(!el)return;
  el.textContent=txt;
  el.style.color=colour||'#8090a0';
}
function attachDiag(el){
  el.addEventListener('loadstart',()=>{ console.info('[stream] loadstart'); setStreamState('buffering','#c0a050'); });
  el.addEventListener('waiting',  ()=>{ console.info('[stream] waiting');   setStreamState('buffering','#c0a050'); });
  el.addEventListener('stalled',  ()=>{ console.warn('[stream] stalled');   setStreamState('stalled','#c05050'); });
  el.addEventListener('canplay',  ()=>{ console.info('[stream] canplay');   setStreamState('ready','#5cb870'); });
  el.addEventListener('playing',  ()=>{ console.info('[stream] playing');   setStreamState('playing','#5cb870'); });
  el.addEventListener('pause',    ()=>{ console.info('[stream] pause');     if(streamOn) setStreamState('paused','#8090a0'); });
  el.addEventListener('error',()=>{
    const err=el.error||{};
    const code=err.code||0;
    const name=MEDIA_ERR[code]||'unknown';
    console.error('[stream] audio error:',name,'| src:',el.currentSrc,'| readyState=',el.readyState,'networkState=',el.networkState);
    setStreamState('error: '+name,'#c05050');
  });
}
function toggleListen(){
  const el=document.getElementById('stream-audio');
  const btn=document.getElementById('listen-btn');
  if(streamOn){
    el.pause();
    el.src='';
    btn.innerHTML='&#128266;';
    btn.title='Listen in browser';
    streamOn=false;
    setStreamState('idle');
    return;
  }
  // Simplest possible form: bare .src assignment + .play(). Every
  // fancier pattern we tried (<source> child, explicit load(),
  // preload=none, removeAttribute dance) caused problems in one
  // browser or another. `.src=` + `.play()` is what the working
  // direct-URL case boils down to inside the browser, so we mirror it.
  //
  // Diagnostic listeners are re-attached once on first click; they
  // stay for the page's lifetime and drive the state indicator.
  if(!el._diagAttached){ attachDiag(el); el._diagAttached=true; }
  el.src='/api/stream.mp3?t='+Date.now();
  btn.innerHTML='&#128263;';
  btn.title='Stop listening';
  streamOn=true;
  setStreamState('connecting…','#c0a050');
  const pp=el.play();
  if(pp&&pp.catch){
    pp.catch(e=>{
      console.error('[stream] play() rejected:',e.name,e.message);
      setStreamState('blocked: '+e.name,'#c05050');
    });
  }
  setupMediaSession();
}

function setupMediaSession(){
  if(!('mediaSession' in navigator)) return;
  navigator.mediaSession.setActionHandler('play',   ()=>cmd('pause'));
  navigator.mediaSession.setActionHandler('pause',  ()=>cmd('pause'));
  navigator.mediaSession.setActionHandler('previoustrack', ()=>cmd('prev'));
  navigator.mediaSession.setActionHandler('nexttrack',     ()=>cmd('next'));
  navigator.mediaSession.setActionHandler('stop', ()=>cmd('stop'));
}

// Push current-track metadata + timeline into the OS media session
// on every poll — that's what makes lock-screen media widgets show
// title/author + progress. Only actually pokes the API when
// something changed, to avoid churning the OS side every second.
let _mediaLastKey='';
function updateMediaSession(){
  if(!('mediaSession' in navigator)) return;
  const title=(status.title||'—');
  const author=(status.author||'');
  const key=title+'||'+author+'||'+status.state;
  if(key!==_mediaLastKey){
    _mediaLastKey=key;
    try{
      navigator.mediaSession.metadata=new MediaMetadata({
        title: title,
        artist: author,
        album: (status.released||''),
      });
    }catch(_){}
    navigator.mediaSession.playbackState=
      status.state==='playing' ? 'playing'
      : status.state==='paused' ? 'paused'
      : 'none';
  }
  // Position state — allow the OS to render its progress bar. Wrap
  // in try/catch because Firefox throws on invalid combinations.
  if(navigator.mediaSession.setPositionState && status.duration_secs){
    try{
      navigator.mediaSession.setPositionState({
        duration: status.duration_secs,
        position: Math.min(status.elapsed_secs||0, status.duration_secs),
        playbackRate: 1.0,
      });
    }catch(_){}
  }
}

async function pollStreamStatus(){
  try{
    const r=await fetch('/api/stream/status');
    const j=await r.json();
    streamEnabled=!!j.enabled;
    streamAvailable=!!j.available;
    const btn=document.getElementById('listen-btn');
    if(!btn)return;
    // Button clickability depends ONLY on the config flag. Whether
    // audio is currently flowing (`available`) is informational —
    // silence is a legitimate thing to stream. Previously we were
    // disabling the button whenever nothing was playing on the
    // desktop, which made it impossible to start listening before
    // starting a tune.
    btn.disabled=!streamEnabled;
    btn.style.opacity=streamEnabled?'1':'0.4';
    if(!streamOn){
      if(!streamEnabled){
        btn.title='Audio streaming disabled — enable in Settings → Network';
        setStreamState('disabled');
      } else {
        btn.title=streamAvailable?'Listen in browser':'Listen in browser (silence — nothing playing)';
        setStreamState(streamAvailable?'idle':'idle (no audio)');
      }
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
  if(ev) ev.stopPropagation();
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
  document.getElementById('lt-lk').classList.toggle('active',which==='lk');
  document.getElementById('lt-hv').classList.toggle('active',which==='hv');
  document.getElementById('lt-rc').classList.toggle('active',which==='rc');
  document.getElementById('lib-pl').style.display=which==='pl'?'block':'none';
  document.getElementById('lib-lk').style.display=which==='lk'?'block':'none';
  document.getElementById('lib-hv').style.display=which==='hv'?'block':'none';
  document.getElementById('lib-rc').style.display=which==='rc'?'block':'none';
  if(which==='hv'&&!hvscAuthor){loadHvscAuthors();}
  if(which==='rc'){loadRecentlyPlayed();}
  if(which==='lk'){loadLiked();}
}

// ── Liked collection tab ──────────────────────────────────────
// The primary "Liked destination" — modeled on Spotify's Liked
// Songs. Header shows total + Play-all; each row plays that one
// track and can unlike from here without leaving the tab.
async function loadLiked(){
  const hdr=document.getElementById('lib-lk-header');
  const list=document.getElementById('lib-lk-list');
  list.innerHTML='<div class="lib-empty">Loading…</div>';
  hdr.innerHTML='';
  try{
    const r=await fetch('/api/favorites');
    const rows=await r.json();
    if(!rows||rows.length===0){
      hdr.innerHTML='';
      list.innerHTML='<div class="lib-empty">Nothing liked yet. Tap ♥ next to any track to add it here.</div>';
      return;
    }
    hdr.innerHTML='<div class="lib-header-row">'+
      '<div class="lib-header-title">'+rows.length+' liked track'+(rows.length===1?'':'s')+'</div>'+
      '<button class="tb-btn" onclick="playAllLiked()">▶ Play all liked</button>'+
      '</div>';
    list.innerHTML=rows.map(t=>{
      const dur=t.duration_secs?fmtTime(t.duration_secs):'';
      const author=t.author?' <span style="color:#607080;">— '+esc(t.author)+'</span>':'';
      return '<div class="lib-row" data-lk-idx="'+t.index+'">'+
        '<div class="lib-name">'+esc(t.title||'(untitled)')+author+'</div>'+
        '<span class="lib-meta">'+dur+'</span>'+
        '<button class="lk-row-play" data-lk-play="'+t.index+'" title="Play this track">▶</button>'+
        '</div>';
    }).join('');
    // Wire click delegation for row + play button.
    list.querySelectorAll('[data-lk-idx]').forEach(row=>{
      const idx=parseInt(row.getAttribute('data-lk-idx'));
      row.addEventListener('click',(e)=>{
        if(e.target.closest('button')) return;
        playLikedRow(idx);
      });
    });
    list.querySelectorAll('[data-lk-play]').forEach(btn=>{
      const idx=parseInt(btn.getAttribute('data-lk-play'));
      btn.addEventListener('click',(e)=>{ e.stopPropagation(); playLikedRow(idx); });
    });
  }catch(e){
    list.innerHTML='<div class="lib-empty">Failed to load Liked collection.</div>';
  }
}
async function playAllLiked(){
  const r=await fetch('/api/favorites/play',{method:'POST'});
  if(r.ok){ toast('Loaded all liked'); toggleLibrary(); setTimeout(poll,200); }
}
async function playLikedRow(idx){
  const r=await fetch('/api/favorites/play/'+idx,{method:'POST'});
  if(r.ok){ toast('Playing from Liked'); setTimeout(poll,200); }
  else { toast('Track not resolvable — try Play all liked','danger'); }
}
// Convenience: open the Library panel focused on the Liked tab
// (used by the empty-state button in the playlist view).
function openLikedTab(){
  const lib=document.getElementById('lib');
  if(lib && lib.style.display==='none') toggleLibrary();
  showLibTab('lk');
}

// ── Settings drawer ─────────────────────────────────────────────
function toggleSettings(){
  const p=document.getElementById('settings-panel');
  if(!p) return;
  p.style.display=p.style.display==='flex'?'none':'flex';
}
async function postSettings(kind){
  const url=kind==='skip-rsid'?'/api/settings/skip-rsid'
    :kind==='force-stereo'?'/api/settings/force-stereo':null;
  if(!url) return;
  const r=await fetch(url,{method:'POST'});
  if(r.ok){ setTimeout(poll,120); }
}
async function setSurpriseSource(src){
  const r=await fetch('/api/settings/surprise-source/'+src,{method:'POST'});
  if(r.ok){ toast('Surprise source: '+src); setTimeout(poll,120); }
}
async function setRepeat(m){
  const r=await fetch('/api/repeat/'+m,{method:'POST'});
  if(r.ok){ toast('Repeat: '+m); setTimeout(poll,120); }
}
async function setShuffle(on){
  const r=await fetch('/api/shuffle/'+(on?'on':'off'),{method:'POST'});
  if(r.ok){ toast('Shuffle: '+(on?'on':'off')); setTimeout(poll,120); }
}

// Reflect current settings state on the drawer. Called from poll().
function updateSettingsDrawer(){
  const s=status||{};
  const skip=document.getElementById('set-skip-rsid');
  if(skip){
    skip.textContent=s.skip_rsid?'On':'Off';
    skip.classList.toggle('on',!!s.skip_rsid);
  }
  const fs=document.getElementById('set-force-stereo');
  if(fs){
    fs.textContent=s.force_stereo_2sid?'On':'Off';
    fs.classList.toggle('on',!!s.force_stereo_2sid);
  }
  const surp=s.surprise_source||'hvsc';
  const surpH=document.getElementById('set-surp-hvsc');
  const surpP=document.getElementById('set-surp-pl');
  if(surpH) surpH.classList.toggle('on',surp==='hvsc');
  if(surpP) surpP.classList.toggle('on',surp==='playlist');
  document.querySelectorAll('[data-rep]').forEach(b=>{
    b.classList.toggle('on', b.getAttribute('data-rep')===(s.repeat||'off'));
  });
  document.querySelectorAll('[data-shuf]').forEach(b=>{
    const want=(b.getAttribute('data-shuf')==='on');
    b.classList.toggle('on', !!s.shuffle===want);
  });
}

async function toggleHvscSync(){
  const active=!!(status && status.hvsc_sync_active);
  const url=active?'/api/library/hvsc/sync/cancel':'/api/library/hvsc/sync/start';
  const r=await fetch(url,{method:'POST'});
  if(r.ok){ toast(active?'Sync cancelled':'HVSC sync started'); setTimeout(poll,150); }
}

// Reflect the current sync state on the HVSC tab. Called from poll().
function updateHvscSync(){
  const btn=document.getElementById('hv-sync-btn');
  const stat=document.getElementById('hv-sync-status');
  if(!btn||!stat) return;
  const active=!!(status && status.hvsc_sync_active);
  btn.innerHTML=active?'✕ Cancel sync':'↓ Sync HVSC';
  btn.classList.toggle('danger',active);
  const p=status && status.hvsc_sync_progress;
  if(active && p && p.length===2 && p[1]>0){
    const pct=Math.round(100*p[0]/p[1]);
    stat.textContent='syncing… '+p[0]+'/'+p[1]+' ('+pct+'%)';
  } else if(active){
    stat.textContent='syncing…';
  } else {
    stat.textContent='';
  }
}

async function loadRecentlyPlayed(){
  const el=document.getElementById('lib-rc-list');
  el.innerHTML='<div class="lib-empty">Loading…</div>';
  try{
    const r=await fetch('/api/recent');
    const list=await r.json();
    if(!list||list.length===0){
      el.innerHTML='<div class="lib-empty">Nothing played yet.</div>';
      return;
    }
    el.innerHTML=list.map(e=>
      '<div class="lib-row" onclick="playRecent('+e.index+')">'+
      '<div class="lib-name">'+esc(e.title||'')+
        (e.author?' <span style="color:#607080;">— '+esc(e.author)+'</span>':'')+
      '</div>'+
      '<div class="lib-meta">'+esc(e.played_at_relative||'')+'</div>'+
      '</div>').join('');
  }catch(e){
    el.innerHTML='<div class="lib-empty">Failed to load history.</div>';
  }
}
async function playRecent(idx){
  const r=await fetch('/api/recent/play/'+idx,{method:'POST'});
  if(r.ok){ toast('Playing from history'); setTimeout(poll,200); }
  else { toast('Track not found','danger'); }
}

// Which published playlist is currently expanded to preview, and a
// cache of its parsed track list so re-opening is instant.
let expandedPub=null;
const pubPreviewCache={};

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
    // Each row shows the playlist header. Clicking the row expands
    // an inline preview; the ▶ Load button loads the whole thing.
    el.innerHTML=list.map(p=>{
      const name=esc(p.name||p.file);
      const desc=p.description?'<div style="font-size:11px;color:#607080;margin-top:2px;">'+esc(p.description)+'</div>':'';
      const tracks=p.tracks?p.tracks+' tracks':'';
      const file=esc(p.file);
      return '<div class="pub-item" data-file="'+file+'">'+
        '<div class="lib-row" onclick="togglePub(\''+file+'\')">'+
          '<div><div class="lib-name">'+name+'</div>'+desc+'</div>'+
          '<span class="lib-meta">'+tracks+'</span>'+
        '</div>'+
        '<div class="pub-preview" id="pub-prev-'+file+'" style="display:none;">'+
          '<div class="pub-preview-actions">'+
            '<button class="tb-btn" onclick="loadPub(\''+file+'\')">▶ Load this playlist</button>'+
          '</div>'+
          '<div class="pub-preview-body" id="pub-prev-body-'+file+'">'+
            '<div class="lib-empty">Loading…</div>'+
          '</div>'+
        '</div>'+
      '</div>';
    }).join('');
  }catch(e){
    el.innerHTML='<div class="lib-empty">Failed to load: '+esc(e.message||'error')+'</div>';
  }
}

async function togglePub(file){
  // Collapse the currently expanded row first (only one open at a
  // time — matches the desktop's accordion behaviour).
  if(expandedPub && expandedPub!==file){
    const prev=document.getElementById('pub-prev-'+expandedPub);
    if(prev) prev.style.display='none';
  }
  const el=document.getElementById('pub-prev-'+file);
  if(!el) return;
  if(el.style.display==='block'){
    el.style.display='none';
    expandedPub=null;
    return;
  }
  el.style.display='block';
  expandedPub=file;
  // Cached? Skip the fetch.
  if(pubPreviewCache[file]){
    renderPubPreview(file, pubPreviewCache[file]);
    return;
  }
  try{
    const r=await fetch('/api/library/playlists/preview/'+encodeURIComponent(file));
    if(!r.ok){
      document.getElementById('pub-prev-body-'+file).innerHTML=
        '<div class="lib-empty">Preview not available (not yet downloaded — Load it once, then preview).</div>';
      return;
    }
    const tracks=await r.json();
    pubPreviewCache[file]=tracks;
    renderPubPreview(file, tracks);
  }catch(e){
    document.getElementById('pub-prev-body-'+file).innerHTML=
      '<div class="lib-empty">Preview failed.</div>';
  }
}

function renderPubPreview(file, tracks){
  const body=document.getElementById('pub-prev-body-'+file);
  if(!body) return;
  if(!tracks || tracks.length===0){
    body.innerHTML='<div class="lib-empty">This playlist is empty.</div>';
    return;
  }
  body.innerHTML=tracks.map((t,i)=>{
    const dur=t.duration_secs?fmtTime(t.duration_secs):'';
    const author=t.author?' <span style="color:#607080;">— '+esc(t.author)+'</span>':'';
    return '<div class="pub-preview-track"><span class="pub-preview-idx">'+
      (i+1)+'.</span> <span>'+esc(t.title||'')+author+'</span>'+
      '<span class="pub-preview-dur">'+dur+'</span></div>';
  }).join('');
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
        '<button onclick="addHvsc(\''+p+'\',event)" title="Add this tune to the current playlist without playing" style="background:none;border:1px solid #2a2e36;color:#5cb870;padding:4px 8px;border-radius:4px;cursor:pointer;font-size:11px;">+</button>'+
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
    // Subtune stepper: only show when the current tune has >1 song.
    const stepper=document.getElementById('subtune-stepper');
    if(stepper){
      const songs=status.songs||0;
      const cur=status.current_song||0;
      if(songs>1){
        stepper.style.display='inline-flex';
        document.getElementById('subtune-badge').textContent=cur+'/'+songs;
      } else {
        stepper.style.display='none';
      }
    }
    // Mobile Now Playing bar mirrors the top-of-page player.
    const npmTitle=document.getElementById('npm-title');
    if(npmTitle){
      npmTitle.textContent=status.title||'\u2014';
      document.getElementById('npm-author').textContent=status.author||'';
      document.getElementById('npm-pp').innerHTML=
        status.state==='playing'?'\u23F8':'\u25B6';
      document.getElementById('npm-prog').style.width=
        ((status.duration_secs&&status.duration_secs>0)?
          Math.min(100,(status.elapsed_secs/status.duration_secs)*100):0)+'%';
    }
    // OS media session \u2014 title, author, position. Lock-screen +
    // notification-shade controls "just work" once these are set.
    updateMediaSession();
    updateHvscSync();
    updateSettingsDrawer();
    if(status.current_index!==curIdx){curIdx=status.current_index;highlightCurrent();}
    // Auto-refresh playlist when the desktop side changes it
    // (Surprise Me, drag-add, folder import, favourite toggle).
    if(status.playlist_version!==undefined){
      if(lastPlaylistVersion===null){
        lastPlaylistVersion=status.playlist_version;
      } else if(status.playlist_version!==lastPlaylistVersion){
        lastPlaylistVersion=status.playlist_version;
        // Server changed the playlist — animate the incoming diff.
        // The View Transitions wrapper morphs old→new rows.
        if(document.startViewTransition){
          document.startViewTransition(()=>loadPlaylist(false));
        } else {
          loadPlaylist(false);
        }
      }
    }
    // Now-playing heart reflects current-track Liked state.
    const heart=document.getElementById('np-heart');
    if(heart){
      heart.innerHTML=status.is_favorite?'♥':'♡';
      heart.classList.toggle('on',!!status.is_favorite);
      heart.title=status.is_favorite?'Remove from Liked':'Add to Liked';
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
  // First-load skeleton so the browser has something to render
  // while the JSON is in flight. Only on fresh loads (not append)
  // and only when we don't already have entries to show.
  if(!append && entries.length===0){
    const el=document.getElementById('pl');
    if(el){
      el.innerHTML=Array.from({length:6}).map(()=>
        '<div class="skeleton-row">'+
        '<div class="skeleton-bar skeleton-idx"></div>'+
        '<div class="skeleton-bar skeleton-title"></div>'+
        '<div class="skeleton-bar skeleton-dur"></div>'+
        '</div>').join('');
    }
  }
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

// State for the favourites-only chip. Purely client-side — the
// server already sends `is_favorite` per entry so no round-trip.
let favOnly=false;

function renderList(totalAll){
  const el=document.getElementById('pl');
  const info=document.getElementById('pl-info');
  const q=document.getElementById('q').value;
  const visible = favOnly ? entries.filter(t=>t.is_favorite) : entries;
  if(q){info.textContent=total+' matches (showing '+visible.length+') of '+totalAll+' total';}
  else if(favOnly){info.textContent='♥ '+visible.length+' liked / '+entries.length+' loaded';}
  else{info.textContent=entries.length+' of '+total+' tracks';}
  // Friendly empty state: don't just show a blank list, give the
  // user the three ways to fill it (matches the modern-player
  // convention for "no results" pages).
  if(visible.length===0 && !loading){
    if(favOnly){
      el.innerHTML='<div class="pl-empty">'+
        '<div class="em-hint">No liked tracks in this playlist.</div>'+
        '<div class="em-actions">'+
        '<button onclick="toggleFavOnly()">Show all tracks</button>'+
        '<button onclick="openLikedTab()">❤ Open Liked collection</button>'+
        '</div></div>';
    } else if(q){
      el.innerHTML='<div class="pl-empty">'+
        '<div class="em-hint">No tracks match "'+esc(q)+'".</div>'+
        '<div class="em-actions">'+
        '<button onclick="clearSearch()">Clear search</button>'+
        '</div></div>';
    } else {
      el.innerHTML='<div class="pl-empty">'+
        '<div class="em-hint">Your playlist is empty.</div>'+
        '<div class="em-actions">'+
        '<button onclick="openLikedTab()">❤ Open Liked collection</button>'+
        '<button onclick="toggleLibrary()">📚 Open Library</button>'+
        '<button onclick="pickImportM3U()">↑ Import M3U</button>'+
        '</div></div>';
    }
    return;
  }
  el.innerHTML=visible.map(t=>{
    const active=t.index===curIdx?'active':'';
    const dur=t.duration?fmtTime(t.duration):'';
    const favClass=t.is_favorite?'heart on':'heart';
    const favGlyph=t.is_favorite?'♥':'♡';
    return '<div class="track '+active+'" data-idx="'+t.index+'" draggable="true">'+
      '<span class="idx">'+(t.index+1)+'</span>'+
      '<div class="info"><div class="t-title">'+esc(t.title)+'</div>'+
      '<div class="t-author">'+esc(t.author)+'</div></div>'+
      '<span class="dur">'+dur+'</span>'+
      // Row-level heart lives INSIDE the hover-actions cluster so it
      // no longer pollutes idle rows. A filled \u2665 shows even at rest
      // if the track is already liked (that's the "state indicator"
      // \u2014 quiet but present, matches Apple Music's style).
      '<div class="actions">'+
      '<button class="'+favClass+'" data-role="fav" title="'+
        (t.is_favorite?'Remove from Liked':'Add to Liked')+'">'+favGlyph+'</button>'+
      '<button data-role="menu" title="More actions">\u22ee</button>'+
      '</div>'+
      '</div>';
  }).join('');
  // Show "load more" only when we're not filtering client-side \u2014
  // server pagination is over the full set, not the filtered subset.
  if(!favOnly && entries.length<total){
    el.innerHTML+='<div class="track" data-role="loadmore" '+
      'style="justify-content:center;color:#5cb870;font-size:13px;">'+
      '\u25bc Load more ('+entries.length+'/'+total+')</div>';
  }
  wirePlaylistEvents();
}

function highlightCurrent(){
  document.querySelectorAll('.track').forEach(el=>{
    const idx=parseInt(el.querySelector('.idx')?.textContent)-1;
    el.classList.toggle('active',idx===curIdx);
  });
  const active=document.querySelector('.track.active');
  if(active)active.scrollIntoView({block:'nearest',behavior:'smooth'});
}

// ── Row event wiring, run after every renderList() ──────────────
// One place for click / right-click / long-press / drag on rows —
// keeps the render function pure HTML and centralises the state
// (dragFromIdx) here so a mid-drag re-render can't stale it.
let dragFromIdx=null;
function wirePlaylistEvents(){
  const rows=document.querySelectorAll('#pl .track');
  rows.forEach(row=>{
    if(row.getAttribute('data-role')==='loadmore'){
      row.addEventListener('click',()=>loadPlaylist(true));
      return;
    }
    const idx=parseInt(row.getAttribute('data-idx'));
    if(isNaN(idx)) return;
    row.addEventListener('click',(e)=>{
      if(e.target.closest('button')) return;
      playIdx(idx);
    });
    row.querySelector('[data-role=fav]')?.addEventListener('click',(e)=>{
      e.stopPropagation();
      toggleFav(idx);
    });
    row.querySelector('[data-role=menu]')?.addEventListener('click',(e)=>{
      e.stopPropagation();
      const r=row.getBoundingClientRect();
      openCtxMenu(idx, r.right-8, r.top+r.height);
    });
    row.addEventListener('contextmenu',(e)=>{
      e.preventDefault();
      openCtxMenu(idx, e.clientX, e.clientY);
    });
    // Touch long-press = context menu. 500 ms is the community
    // norm — long enough to disambiguate from a tap, short enough
    // to feel responsive.
    let pressT=null;
    row.addEventListener('touchstart',(e)=>{
      pressT=setTimeout(()=>{
        const t=e.touches[0];
        openCtxMenu(idx, t.clientX, t.clientY);
      }, 500);
    },{passive:true});
    row.addEventListener('touchend',()=>{ if(pressT){clearTimeout(pressT);pressT=null;} });
    row.addEventListener('touchmove',()=>{ if(pressT){clearTimeout(pressT);pressT=null;} });
    // HTML5 desktop drag. Touch reorder uses the ctx menu's up/down
    // items (drag-on-touch conflicts with scroll — worth a
    // dedicated follow-up).
    row.addEventListener('dragstart',(e)=>{
      dragFromIdx=idx;
      row.classList.add('dragging');
      e.dataTransfer.effectAllowed='move';
      // Firefox refuses to start a drag without a payload.
      e.dataTransfer.setData('text/plain', String(idx));
    });
    row.addEventListener('dragend',()=>{
      row.classList.remove('dragging');
      document.querySelectorAll('.drag-over').forEach(x=>x.classList.remove('drag-over'));
      dragFromIdx=null;
    });
    row.addEventListener('dragover',(e)=>{
      if(dragFromIdx===null||dragFromIdx===idx) return;
      e.preventDefault();
      row.classList.add('drag-over');
    });
    row.addEventListener('dragleave',()=>row.classList.remove('drag-over'));
    row.addEventListener('drop',(e)=>{
      e.preventDefault();
      row.classList.remove('drag-over');
      if(dragFromIdx===null||dragFromIdx===idx) return;
      movePlaylist(dragFromIdx, idx);
    });
  });
}

// ── Toolbar actions ──────────────────────────────────────────────
function pickImportM3U(){ document.getElementById('import-m3u-input').click(); }
async function onImportM3U(ev){
  const file=ev.target.files&&ev.target.files[0];
  if(!file){ ev.target.value=''; return; }
  try{
    const text=await file.text();
    const r=await fetch('/api/playlist/import',{method:'POST',body:text});
    if(r.ok){ toast('Imported "'+file.name+'"'); setTimeout(()=>loadPlaylist(false),300); }
    else { toast('Import failed: '+r.status,'danger'); }
  }catch(e){ console.error(e); toast('Import error','danger'); }
  ev.target.value='';
}
async function clearPlaylist(){
  if(!confirm('Clear the entire playlist? Liked tracks (❤) are kept.')) return;
  const r=await fetch('/api/playlist/clear',{method:'POST'});
  if(r.ok){ toast('Playlist cleared'); setTimeout(()=>loadPlaylist(false),200); }
}
// Small wrapper — if the browser supports View Transitions the
// snapshot morphs from old to new state; otherwise it's a direct
// call.  Used by any client-side mutation that changes what the
// playlist renders (fav-only toggle, drag reorder, sort, etc).
function animate(mut){
  if(document.startViewTransition){
    document.startViewTransition(mut);
  } else {
    mut();
  }
}
function toggleFavOnly(){
  favOnly=!favOnly;
  const chip=document.getElementById('fav-only-chip');
  chip.classList.toggle('active',favOnly);
  // Same filter icon in both states — no heart glyph on this chip
  // any more (chip is a filter, not a like action). Active state
  // colour is what tells the user it's engaged.
  chip.innerHTML='☰ Liked only';
  animate(()=>renderList(total));
}
async function sortPlaylist(col){
  if(!col) return;
  const sel=document.getElementById('sort-select');
  const r=await fetch('/api/playlist/sort/'+encodeURIComponent(col),{method:'POST'});
  if(r.ok){ toast('Sorted by '+col); setTimeout(()=>loadPlaylist(false),200); }
  sel.value='';
}
async function removeIdx(idx){
  const r=await fetch('/api/playlist/remove/'+idx,{method:'POST'});
  if(r.ok){ toast('Removed'); setTimeout(()=>loadPlaylist(false),150); }
}
async function movePlaylist(from,to){
  // Optimistic: reorder locally first. Server's playlist_version
  // bump triggers a canonical re-fetch, so any drift self-heals.
  const iFrom=entries.findIndex(e=>e.index===from);
  const iTo=entries.findIndex(e=>e.index===to);
  if(iFrom>=0 && iTo>=0){
    const [moved]=entries.splice(iFrom,1);
    entries.splice(iTo,0,moved);
    animate(()=>renderList(total));
  }
  const r=await fetch('/api/playlist/move',{method:'POST',body:JSON.stringify({from,to})});
  if(!r.ok){ toast('Move failed — reverting','danger'); loadPlaylist(false); }
}
async function playIdx(idx){
  await fetch('/api/play/'+idx,{method:'POST'});
  setTimeout(poll,150);
}
function clearSearch(){
  const q=document.getElementById('q');
  if(q){ q.value=''; onSearch(); }
}

// ── Context menu ─────────────────────────────────────────────────
let ctxIdx=null;
function openCtxMenu(idx,x,y){
  ctxIdx=idx;
  const m=document.getElementById('ctx-menu');
  const t=entries.find(e=>e.index===idx);
  document.getElementById('ctx-fav-label').textContent=
    (t&&t.is_favorite)?'Remove from Liked':'Add to Liked';
  m.style.display='block';
  const rect=m.getBoundingClientRect();
  const w=rect.width||220, h=rect.height||300;
  m.style.left=Math.min(x, window.innerWidth - w - 8)+'px';
  m.style.top=Math.min(y, window.innerHeight - h - 8)+'px';
}
function closeCtxMenu(){
  document.getElementById('ctx-menu').style.display='none';
  ctxIdx=null;
}
document.addEventListener('click',(e)=>{
  if(!e.target.closest('#ctx-menu')) closeCtxMenu();
});
document.addEventListener('scroll',closeCtxMenu,true);
async function ctxAction(kind){
  const idx=ctxIdx;
  closeCtxMenu();
  if(idx===null||idx===undefined) return;
  switch(kind){
    case 'play':   playIdx(idx); break;
    case 'fav':    toggleFav(idx); break;
    case 'top':    if(idx>0){ movePlaylist(idx, 0); toast('Moved to top'); } break;
    case 'up':     if(idx>0) movePlaylist(idx, idx-1); break;
    case 'down':   if(idx<total-1) movePlaylist(idx, idx+1); break;
    case 'copy':   {
      const t=entries.find(e=>e.index===idx);
      if(t){
        const s=(t.author?t.author+' — ':'')+t.title;
        try{ await navigator.clipboard.writeText(s); toast('Copied "'+s+'"'); }
        catch(_){ toast('Copy blocked by browser','danger'); }
      }
      break;
    }
    case 'remove': removeIdx(idx); break;
  }
}

// ── Toast notifications ──────────────────────────────────────────
function toast(msg,kind){
  const stack=document.getElementById('toast-stack');
  if(!stack) return;
  const t=document.createElement('div');
  t.className='toast'+(kind==='danger'?' danger':'');
  t.textContent=msg;
  stack.appendChild(t);
  setTimeout(()=>{ t.style.transition='opacity 0.2s'; t.style.opacity='0';
    setTimeout(()=>t.remove(),220); }, 2200);
}

// Auto-load more when scrolling near bottom
document.getElementById('pl').addEventListener('scroll',function(){
  if(this.scrollTop+this.clientHeight>=this.scrollHeight-50){
    if(entries.length<total)loadPlaylist(true);
  }
});

// ── Global keyboard shortcuts ──────────────────────────────────
// Mirror the desktop bindings so the same muscle memory works when
// the web UI is on the same machine's browser. Skipped while typing
// in the search box (so `/`, `Space`, `Escape` etc. behave
// naturally there).
document.addEventListener('keydown',(e)=>{
  const t=e.target;
  if(t && (t.tagName==='INPUT'||t.tagName==='TEXTAREA'||t.tagName==='SELECT'||t.isContentEditable)){
    // Escape blurs the search field — same as desktop.
    if(e.key==='Escape' && t.tagName==='INPUT'){ t.blur(); }
    return;
  }
  if(e.metaKey||e.ctrlKey||e.altKey) return;
  switch(e.key){
    case ' ':      e.preventDefault(); cmd('pause'); break;
    case 'ArrowRight': e.preventDefault(); cmd('next'); break;
    case 'ArrowLeft':  e.preventDefault(); cmd('prev'); break;
    case 'f':
    case 'F':      e.preventDefault(); toggleFavCurrent(); break;
    case 'h':
    case 'H':      e.preventDefault(); toggleFavCurrent(); break;
    case '/':      e.preventDefault(); document.getElementById('q')?.focus(); break;
    case 's':
    case 'S':      e.preventDefault(); cmd('surprise'); break;
    case 'l':
    case 'L':      e.preventDefault(); toggleLibrary(); break;
    case 'r':
    case 'R':      e.preventDefault(); cmd('shuffle'); break;
    case '?':      e.preventDefault(); showHelpOverlay(); break;
  }
});

// Minimal in-browser help overlay listing the bindings above. Built
// once, toggled on demand. Dismissed by any click.
function showHelpOverlay(){
  let ov=document.getElementById('help-overlay');
  if(!ov){
    ov=document.createElement('div');
    ov.id='help-overlay';
    ov.style.cssText='position:fixed;inset:0;background:rgba(0,0,0,0.7);z-index:1100;'+
      'display:flex;align-items:center;justify-content:center;font-size:13px;';
    ov.innerHTML='<div style="background:#1a1e26;border:1px solid #2a2e36;'+
      'border-radius:8px;padding:20px 26px;max-width:340px;color:#c8ccd0;">'+
      '<div style="font-size:15px;color:#5cb870;margin-bottom:10px;">Keyboard shortcuts</div>'+
      '<div style="display:grid;grid-template-columns:80px 1fr;gap:6px 12px;">'+
      '<code>Space</code><span>Play / Pause</span>'+
      '<code>← / →</code><span>Previous / Next track</span>'+
      '<code>F or H</code><span>Toggle Liked</span>'+
      '<code>S</code><span>Surprise Me</span>'+
      '<code>R</code><span>Toggle shuffle</span>'+
      '<code>L</code><span>Toggle Library</span>'+
      '<code>/</code><span>Focus search</span>'+
      '<code>?</code><span>This help</span>'+
      '</div><div style="margin-top:12px;color:#8090a0;">Click anywhere to close.</div></div>';
    ov.addEventListener('click',()=>ov.remove());
    document.body.appendChild(ov);
  }
}

setInterval(poll,1000);
poll();
loadPlaylist(false);
</script>
</body>
</html>
"##;
