// HVSC sync via gosh-dl's recursive-http engine, pipelined per top-level subtree.
//
// Strategy:
//   1. Fetch the root HTML index ONCE with reqwest, parse out top-level
//      subdirectories (DEMOS/, DOCUMENTS/, GAMES/, MUSICIANS/, ...).
//   2. Spawn one discovery+download task per subtree, running concurrently.
//      Downloads start as soon as the FIRST subtree's manifest is built —
//      we don't wait for the entire ~75k-file tree to be enumerated.
//   3. Skip files that already exist locally (no HEAD, no GET, no overwrite).
//      This makes re-runs near-instant for unchanged content.
//   4. Stream aggregate progress: subtrees discovered, files queued, files
//      done, files skipped.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use gosh_dl::{
    DownloadEngine, DownloadEvent, DownloadId, DownloadOptions, DownloadState, EngineConfig,
    RecursiveOptions,
};
use tokio::sync::broadcast::error::RecvError as BroadcastRecvError;
use tokio::sync::Semaphore;

/// Hard cap on how many subtree DISCOVERIES run at the same time.
/// Downloads after discovery aren't counted (they flow through gosh-dl's
/// own queue), so this just controls how aggressively we hammer the
/// mirror's HTML index pages. Community-run mirrors like brona.dk are
/// slow (~3 KB/s); raising this hits TLS handshake / connect timeouts.
const MAX_CONCURRENT_SUBTREE_DISCOVERIES: usize = 4;

const PROGRESS_QUEUE_DEPTH: usize = 64;

/// One event published by the sync worker.
#[derive(Debug, Clone)]
pub enum HvscSyncEvent {
    Progress {
        files_done: u32,
        files_total: u32,
        bytes_done: u64,
        /// Total expected bytes. Always 0 today because HVSC's HTTP
        /// directory index doesn't expose per-file sizes in a form gosh-dl
        /// extracts. Kept on the wire so a future mirror with HEAD-based
        /// size discovery can fill it in without an API break.
        #[allow(dead_code)]
        bytes_total: u64,
        current: String,
    },
    Done(Result<(), String>),
}

/// Handle to an in-progress sync. Dropping it cancels and joins the worker.
pub struct HvscSyncHandle {
    pub rx: Receiver<HvscSyncEvent>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl HvscSyncHandle {
    /// Spawn the sync. Returns immediately; results stream over `rx`.
    pub fn start(url: &str, dest: &Path) -> Result<Self, String> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "URL must be http(s):// (got `{url}`). HVSC sync uses HTTPS \
                 directory crawling."
            ));
        }
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("Cannot create destination {}: {e}", dest.display()))?;

        let (tx, rx) = bounded(PROGRESS_QUEUE_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = Arc::clone(&cancel);
        let dest = dest.to_path_buf();
        // Trim defensively — a trailing space in the configured URL turns
        // every fetched path into `…/%20foo`, which the server 404s on.
        let url = url.trim().to_string();

        let join = thread::Builder::new()
            .name("hvsc-sync".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(HvscSyncEvent::Done(Err(format!(
                            "Cannot build tokio runtime: {e}"
                        ))));
                        return;
                    }
                };
                let result = rt.block_on(run_sync(&url, dest, &tx, &cancel_for_thread));
                if let Err(e) = result {
                    let _ = tx.send(HvscSyncEvent::Done(Err(e)));
                }
            })
            .map_err(|e| format!("Cannot spawn sync thread: {e}"))?;

        Ok(Self {
            rx,
            cancel,
            join: Some(join),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl Drop for HvscSyncHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// One discovered top-level link from the root HTML index.
struct RootLink {
    /// Relative href as it appeared in the index, e.g. "MUSICIANS/" or "readme.1st".
    relative: String,
    /// Absolute URL ready to pass to reqwest / gosh-dl.
    absolute: String,
    /// True if `relative` ends in `/` (directory).
    is_dir: bool,
}

async fn run_sync(
    url: &str,
    dest: PathBuf,
    tx: &Sender<HvscSyncEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let engine = DownloadEngine::new(EngineConfig {
        download_dir: dest.clone(),
        ..EngineConfig::default()
    })
    .await
    .map_err(|e| format!("Cannot start download engine: {e}"))?;
    let mut events = engine.subscribe();

    let options = DownloadOptions {
        save_dir: Some(dest.clone()),
        ..DownloadOptions::default()
    };
    let recursive = RecursiveOptions {
        max_files: Some(50_000),
        max_pages: Some(5_000),
        // Intra-subtree discovery concurrency. Combined with the outer
        // MAX_CONCURRENT_SUBTREE_DISCOVERIES cap (4), total simultaneous
        // HTML index requests = 4 × 4 = 16. Sized for community-run
        // mirrors (brona.dk responds at ~3 KB/s under load — anything
        // above ~20 simultaneous requests starts hitting TLS/connect
        // timeouts). Trades sync wall-clock for reliability.
        max_discovery_concurrency: 4,
        ..RecursiveOptions::default()
    };

    // ── Phase 1: fetch root index, parse top-level links ────────────────────
    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        current: "Listing root directory…".to_string(),
    });
    let root_html = fetch_html(url)
        .await
        .map_err(|e| format!("Cannot fetch {url}: {e}"))?;
    let root_links =
        parse_root_links(url, &root_html).map_err(|e| format!("Cannot parse root index: {e}"))?;

    if root_links.is_empty() {
        return Err(format!(
            "{url} returned an index with no usable links. Wrong URL, or the \
             server is not serving a standard HTML directory listing?"
        ));
    }

    // ── Phase 2: queue root-level files (skip-existing) ─────────────────────
    let mut pending: HashSet<DownloadId> = HashSet::new();
    let mut files_queued: u32 = 0;
    let mut files_done: u32 = 0;
    let mut files_skipped: u32 = 0;
    let mut bytes_done: u64 = 0;
    let mut subtree_errors: u32 = 0;

    // Each subtree task gets the *full* URL to crawl AND the relative
    // prefix (e.g. "MUSICIANS/A") that all its file paths sit under
    // locally. We need the prefix because gosh-dl's RecursiveEntry
    // gives a path relative to the subtree URL, not to our hvsc_root.
    let mut subtree_jobs: Vec<(String, PathBuf)> = Vec::new();

    let mut top_level_dirs: Vec<(String, String)> = Vec::new(); // (relative href, absolute URL)
    for link in &root_links {
        if link.is_dir {
            top_level_dirs.push((link.relative.clone(), link.absolute.clone()));
        } else {
            let local = dest.join(&link.relative);
            if local.exists() {
                files_skipped += 1;
                continue;
            }
            match queue_file(
                &engine,
                &dest,
                &options,
                &link.absolute,
                Path::new(&link.relative),
            )
            .await
            {
                Ok(id) => {
                    pending.insert(id);
                    files_queued += 1;
                }
                Err(e) => {
                    eprintln!("[hvsc-sync] queue {} failed: {e}", link.absolute);
                }
            }
        }
    }

    // ── Phase 2.5: subdivide each top-level dir if it has its own subdirs ──
    // MUSICIANS/ has 26 letter subdirs each with thousands of files. A
    // single discover_http_recursive on MUSICIANS/ would build a 60k+
    // manifest and be sequentially slow. Splitting into MUSICIANS/A/
    // through MUSICIANS/Z/ lets all 26 letter crawls + downloads pipeline.
    // For top-level dirs that don't have subdirs (or have only a handful),
    // we treat the whole dir as one task.
    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        current: "Inspecting top-level subdirectories…".to_string(),
    });
    for (top_rel, top_url) in &top_level_dirs {
        let inner_html = match fetch_html(top_url).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[hvsc-sync] cannot inspect {top_url}: {e}");
                subtree_errors += 1;
                continue;
            }
        };
        let children = parse_root_links(top_url, &inner_html).unwrap_or_default();
        let child_dirs: Vec<&RootLink> = children.iter().filter(|l| l.is_dir).collect();
        if child_dirs.len() >= 4 {
            // Worth splitting — each child becomes its own subtree task.
            for child in &child_dirs {
                let prefix = PathBuf::from(top_rel.trim_end_matches('/'))
                    .join(child.relative.trim_end_matches('/'));
                subtree_jobs.push((child.absolute.clone(), prefix));
            }
            // Files at this level (e.g. MUSICIANS/index.txt if any) — queue directly.
            for child in children.iter().filter(|l| !l.is_dir) {
                let rel = format!("{}{}", top_rel, child.relative);
                let local = dest.join(&rel);
                if local.exists() {
                    files_skipped += 1;
                    continue;
                }
                match queue_file(&engine, &dest, &options, &child.absolute, Path::new(&rel)).await {
                    Ok(id) => {
                        pending.insert(id);
                        files_queued += 1;
                    }
                    Err(e) => eprintln!("[hvsc-sync] queue {} failed: {e}", child.absolute),
                }
            }
        } else {
            // Not many subdirs — crawl the whole top-level dir as one task.
            subtree_jobs.push((
                top_url.clone(),
                PathBuf::from(top_rel.trim_end_matches('/')),
            ));
        }
    }
    let subtrees_total = subtree_jobs.len() as u32;
    let mut subtrees_done: u32 = 0;

    // ── Phase 3: spawn one discovery+enqueue task per subtree ───────────────
    // All N tasks are spawned at once, but only MAX_CONCURRENT_SUBTREE_DISCOVERIES
    // of them hold a permit and run discovery simultaneously. The rest
    // wait their turn on the semaphore. This keeps total HTML in-flight
    // bounded regardless of how many subtrees we discovered above. Once a
    // subtree finishes DISCOVERY it releases the permit (so the next
    // queued subtree can start), and the downloads continue independently
    // via gosh-dl's own engine queue.
    let discovery_permits = Arc::new(Semaphore::new(MAX_CONCURRENT_SUBTREE_DISCOVERIES));
    let mut subtree_set: tokio::task::JoinSet<Result<SubtreeResult, String>> =
        tokio::task::JoinSet::new();
    for (sub_url, prefix) in subtree_jobs {
        let engine_c = engine.clone();
        let dest_c = dest.clone();
        let options_c = options.clone();
        let recursive_c = recursive.clone();
        let cancel_c = Arc::clone(cancel);
        let permits = Arc::clone(&discovery_permits);
        subtree_set.spawn(async move {
            // Wait for a discovery slot. Released automatically when
            // the permit guard is dropped at the end of this task.
            let _permit = permits
                .acquire()
                .await
                .map_err(|e| format!("semaphore closed: {e}"))?;
            discover_and_enqueue(
                &engine_c,
                &dest_c,
                &options_c,
                &recursive_c,
                &sub_url,
                &prefix,
                &cancel_c,
            )
            .await
        });
    }

    // ── Phase 4: main event loop ────────────────────────────────────────────
    let mut last_heartbeat = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            // Abort subtree discovery; cancel queued downloads.
            subtree_set.abort_all();
            engine.cancel_all(false).await;
            let _ = tx.send(HvscSyncEvent::Done(Err("Cancelled".to_string())));
            return Ok(());
        }

        // Terminate: every subtree finished AND every queued download done.
        if subtree_set.is_empty() && pending.is_empty() {
            let err_note = if subtree_errors > 0 {
                format!(" ({} subtree errors — check stderr)", subtree_errors)
            } else {
                String::new()
            };
            let _ = tx.send(HvscSyncEvent::Progress {
                files_done,
                files_total: files_queued,
                bytes_done,
                bytes_total: 0,
                current: format!(
                    "Done. {} new files, {} already present, {} subtrees scanned{}.",
                    files_queued, files_skipped, subtrees_done, err_note
                ),
            });
            let _ = tx.send(HvscSyncEvent::Done(Ok(())));
            return Ok(());
        }

        tokio::select! {
            // Subtree discovery completed.
            subtree_res = subtree_set.join_next(), if !subtree_set.is_empty() => {
                match subtree_res {
                    Some(Ok(Ok(result))) => {
                        for id in result.new_ids {
                            pending.insert(id);
                            files_queued += 1;
                        }
                        files_skipped += result.skipped;
                        subtrees_done += 1;
                    }
                    Some(Ok(Err(e))) => {
                        eprintln!("[hvsc-sync] subtree error: {e}");
                        subtrees_done += 1;
                        subtree_errors += 1;
                    }
                    Some(Err(join_err)) => {
                        eprintln!("[hvsc-sync] subtree task panicked: {join_err}");
                        subtrees_done += 1;
                        subtree_errors += 1;
                    }
                    None => { /* JoinSet empty — handled by the termination check above */ }
                }
            }

            // Download event from any queued file.
            evt = events.recv() => {
                match evt {
                    Ok(DownloadEvent::Completed { id }) if pending.contains(&id) => {
                        pending.remove(&id);
                        files_done += 1;
                        if let Some(status) = engine.status(id) {
                            bytes_done = bytes_done.saturating_add(status.progress.completed_size);
                        }
                    }
                    Ok(DownloadEvent::Failed { id, error, retryable }) if pending.contains(&id) => {
                        pending.remove(&id);
                        files_done += 1;
                        // Look up the URL + on-disk path so the log line is
                        // actually diagnosable (vs an opaque DownloadId).
                        let status = engine.status(id);
                        let (url, save_dir, filename) = status
                            .as_ref()
                            .map(|s| (
                                s.metadata.url.clone().unwrap_or_default(),
                                s.metadata.save_dir.clone(),
                                s.metadata.filename.clone().unwrap_or_default(),
                            ))
                            .unwrap_or_default();
                        eprintln!(
                            "[hvsc-sync] file failed (retryable={retryable}): {filename}  url={url}  err={error}"
                        );
                        // 416 means our local .part has reached or exceeded the
                        // upstream Content-Length (download was already complete
                        // but never got renamed to its final name, or mirror
                        // drift produced a smaller upstream). Delete the stale
                        // partial so the next sync starts fresh and succeeds.
                        if error.contains("416") && !filename.is_empty() {
                            let part = save_dir.join(format!("{filename}.part"));
                            if part.exists() {
                                match std::fs::remove_file(&part) {
                                    Ok(_) => eprintln!(
                                        "[hvsc-sync] removed stale .part for next sync: {}",
                                        part.display()
                                    ),
                                    Err(e) => eprintln!(
                                        "[hvsc-sync] cannot remove {}: {e}",
                                        part.display()
                                    ),
                                }
                            }
                        }
                    }
                    Ok(_) => { /* other event types or unrelated ids */ }
                    // Lagged is recoverable: the broadcast channel had more
                    // events queued than its buffer (heavy completion bursts
                    // with thousands of small files). We may have missed some
                    // Completed/Failed events for items we track — reconcile
                    // by polling engine.status() for every pending id and
                    // promoting any that are now in a terminal state.
                    Err(BroadcastRecvError::Lagged(skipped)) => {
                        eprintln!(
                            "[hvsc-sync] broadcast lagged by {skipped} events; reconciling"
                        );
                        let snapshot: Vec<DownloadId> = pending.iter().copied().collect();
                        for id in snapshot {
                            if let Some(status) = engine.status(id) {
                                match status.state {
                                    DownloadState::Completed => {
                                        pending.remove(&id);
                                        files_done += 1;
                                        bytes_done = bytes_done
                                            .saturating_add(status.progress.completed_size);
                                    }
                                    DownloadState::Error { .. } => {
                                        pending.remove(&id);
                                        files_done += 1;
                                    }
                                    _ => { /* still in progress */ }
                                }
                            } else {
                                // Engine forgot about it (e.g. after cancel_all)
                                // — treat as done to keep the loop moving.
                                pending.remove(&id);
                                files_done += 1;
                            }
                        }
                    }
                    Err(BroadcastRecvError::Closed) => {
                        let _ = tx.send(HvscSyncEvent::Done(Err(
                            "Engine event channel closed unexpectedly".to_string(),
                        )));
                        return Ok(());
                    }
                }
            }

            // Periodic cancel-poll + heartbeat tick.
            _ = tokio::time::sleep(Duration::from_millis(400)) => { /* loop back */ }
        }

        // Emit a heartbeat update every ~1s so the UI shows aggregate
        // progress even when no event just fired.
        if last_heartbeat.elapsed() >= Duration::from_millis(900) {
            last_heartbeat = std::time::Instant::now();
            let current = format!(
                "Subtrees {}/{} scanned, {} new files queued, {} done, {} already present",
                subtrees_done, subtrees_total, files_queued, files_done, files_skipped
            );
            let _ = tx.try_send(HvscSyncEvent::Progress {
                files_done,
                files_total: files_queued,
                bytes_done,
                bytes_total: 0,
                current,
            });
        }
    }
}

struct SubtreeResult {
    new_ids: Vec<DownloadId>,
    skipped: u32,
}

async fn discover_and_enqueue(
    engine: &Arc<DownloadEngine>,
    dest: &Path,
    options: &DownloadOptions,
    recursive: &RecursiveOptions,
    subtree_url: &str,
    prefix: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<SubtreeResult, String> {
    // gosh-dl's crawler doesn't retry transient errors mid-discovery —
    // one network blip on a nested directory page aborts the entire
    // subtree, losing thousands of files. Retry the whole subtree a few
    // times with exponential backoff before giving up on it.
    let manifest = {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_string());
            }
            match engine
                .discover_http_recursive(subtree_url, options, recursive)
                .await
            {
                Ok(m) => break m,
                Err(e) => {
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        return Err(format!(
                            "discover {subtree_url}: {e} (after {attempt} attempts)"
                        ));
                    }
                    let delay = Duration::from_secs(2u64.pow(attempt));
                    eprintln!(
                        "[hvsc-sync] retry {attempt}/{MAX_ATTEMPTS} for {subtree_url} after {e:?}, waiting {}s",
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    };

    let mut new_ids = Vec::new();
    let mut skipped: u32 = 0;
    for entry in &manifest.entries {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        // Full local path = dest / prefix / (entry-relative-to-subtree).
        // gosh-dl's RecursiveEntry.relative_path is relative to subtree_url,
        // not to our hvsc_root, so we have to prepend the subtree prefix.
        let local_rel = prefix.join(&entry.relative_path);
        if skip_if_present(dest, &local_rel, entry.size_hint) {
            skipped += 1;
            continue;
        }
        match queue_file(engine, dest, options, &entry.url, &local_rel).await {
            Ok(id) => new_ids.push(id),
            Err(e) => eprintln!("[hvsc-sync] enqueue {} failed: {e}", entry.url),
        }
    }
    Ok(SubtreeResult { new_ids, skipped })
}

/// Add an HTTP download whose final on-disk path = dest / local_rel.
/// Sets save_dir + filename per-file so gosh-dl preserves the directory tree
/// rather than dumping everything flat into the engine's download_dir.
async fn queue_file(
    engine: &Arc<DownloadEngine>,
    dest: &Path,
    base_options: &DownloadOptions,
    url: &str,
    local_rel: &Path,
) -> Result<DownloadId, String> {
    let absolute = dest.join(local_rel);
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("no parent for {}", absolute.display()))?
        .to_path_buf();
    // Make sure the directory exists before gosh-dl tries to write into it.
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let filename = absolute
        .file_name()
        .ok_or_else(|| format!("no filename for {}", absolute.display()))?
        .to_string_lossy()
        .into_owned();
    let options = DownloadOptions {
        save_dir: Some(parent),
        filename: Some(filename),
        ..base_options.clone()
    };
    engine
        .add_http(url, options)
        .await
        .map_err(|e| format!("add_http {url}: {e}"))
}

/// True if the file is already on disk and we should skip downloading.
fn skip_if_present(dest: &Path, local_rel: &Path, size_hint: Option<u64>) -> bool {
    let local = dest.join(local_rel);
    if !local.exists() {
        return false;
    }
    // If the manifest gave us a size hint, sanity-check; otherwise trust
    // the local file. HVSC's prg.dtu.dk index doesn't include size
    // attributes gosh-dl extracts, so size_hint is usually None and we
    // fall through to "present? skip."
    if let Some(hint) = size_hint {
        if let Ok(meta) = std::fs::metadata(&local) {
            if meta.len() != hint {
                return false;
            }
        }
    }
    true
}

/// Fetch an HTML page via reqwest. Used once per sync for the root index.
async fn fetch_html(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent("phosphor-hvsc-sync/0.4")
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: status {}", resp.status()));
    }
    resp.text().await.map_err(|e| format!("body: {e}"))
}

/// Pull `<a href="…">` targets out of an Apache-style HTML directory listing.
///
/// Filters out:
///   - sort links (Apache adds `?C=N;O=D` etc.)
///   - absolute paths (`/`, `/HVSC/`, etc. — those are parent navigation)
///   - external URLs (`http(s)://other-host/…`)
///   - the parent-directory link (`../`)
fn parse_root_links(base_url: &str, html: &str) -> Result<Vec<RootLink>, String> {
    let base = url::Url::parse(base_url).map_err(|e| format!("invalid base URL: {e}"))?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0usize;

    while let Some(found) = html[pos..].find("href=\"") {
        let start = pos + found + 6;
        let end_off = match html[start..].find('"') {
            Some(e) => e,
            None => break,
        };
        let href = &html[start..start + end_off];
        pos = start + end_off + 1;

        if href.is_empty()
            || href.starts_with('?')
            || href.starts_with('#')
            || href.starts_with('/')
            || href.starts_with("http://")
            || href.starts_with("https://")
            || href == "../"
            || href == ".."
        {
            continue;
        }
        if !seen.insert(href.to_string()) {
            continue;
        }
        let abs = match base.join(href) {
            Ok(u) => u,
            Err(_) => continue,
        };
        // Only keep links that stay under the base path; otherwise we'd
        // chase the host's site-wide navigation.
        if !abs.as_str().starts_with(base.as_str()) {
            continue;
        }
        out.push(RootLink {
            relative: href.to_string(),
            absolute: abs.to_string(),
            is_dir: href.ends_with('/'),
        });
    }
    Ok(out)
}

/// Download a single file from the HVSC base URL into the config directory.
/// Used to refresh DOCUMENTS/Songlengths.md5 and DOCUMENTS/STIL.txt without
/// needing the full tune-tree rsync. The base URL is the same one used for
/// the full sync (`config.hvsc_rsync_url`), so users have a single source
/// of truth for "where is HVSC."
pub async fn fetch_hvsc_document(
    hvsc_base: String,
    relative: &'static str,
    dest: PathBuf,
) -> Result<PathBuf, String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // Build full URL: <base>/DOCUMENTS/<relative>.
    // Trim whitespace first — copy-pasted URLs often have a trailing space
    // that url::Url::parse would percent-encode into `…/%20`, producing a
    // 404 with HTML body that the downstream parser can't make sense of.
    let hvsc_base = hvsc_base.trim();
    let base = if hvsc_base.ends_with('/') {
        hvsc_base.to_string()
    } else {
        format!("{hvsc_base}/")
    };
    let base_url = url::Url::parse(&base).map_err(|e| format!("bad HVSC base URL: {e}"))?;
    let full = base_url
        .join("DOCUMENTS/")
        .and_then(|u| u.join(relative))
        .map_err(|e| format!("URL join failed: {e}"))?;

    let client = reqwest::Client::builder()
        .user_agent("phosphor-hvsc-sync/0.4")
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;
    let resp = client
        .get(full.as_str())
        .send()
        .await
        .map_err(|e| format!("GET {full}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {full}: status {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("body for {full}: {e}"))?;
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Platform-appropriate default destination if `hvsc_root` is unset.
pub fn default_hvsc_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join("Library/Application Support/phosphor/HVSC"))
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|h| h.join(".local/share"))
            })?;
        Some(base.join("phosphor/HVSC"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|h| h.join("phosphor").join("HVSC"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apache_index_links() {
        let html = r#"
        <html><body>
        <a href="?C=N;O=D">sort</a>
        <a href="/">root</a>
        <a href="../">Parent Directory</a>
        <a href="DEMOS/">DEMOS/</a>
        <a href="MUSICIANS/">MUSICIANS/</a>
        <a href="readme.1st">readme.1st</a>
        <a href="https://other.host/x">external</a>
        </body></html>
        "#;
        let links = parse_root_links("https://example.com/HVSC/C64Music/", html).unwrap();
        let relatives: Vec<&str> = links.iter().map(|l| l.relative.as_str()).collect();
        assert_eq!(relatives, vec!["DEMOS/", "MUSICIANS/", "readme.1st"]);
        assert!(links[0].is_dir);
        assert!(!links[2].is_dir);
        assert_eq!(
            links[2].absolute,
            "https://example.com/HVSC/C64Music/readme.1st"
        );
    }
}
