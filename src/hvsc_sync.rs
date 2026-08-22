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

    /// Spawn a "fast first-time sync": download a single archive from `url`
    /// and extract it into `dest`.
    ///
    /// Motivation: the recursive HTTP crawl in [`Self::start`] issues one
    /// GET per file (~75k of them) on a community-run mirror that responds
    /// at a few KB/s. First-time syncs take hours. A single zip of the
    /// full C64Music tree is ~500 MB compressed and pulls in one long-
    /// running stream — orders of magnitude faster for a cold user.
    ///
    /// Behaviour:
    /// * Downloads the archive to a temp file next to `dest` (same
    ///   filesystem, so no cross-device rename).
    /// * Extracts entries whose paths start with `C64Music/` — that prefix
    ///   is *stripped* so files land directly in `dest/DEMOS/`,
    ///   `dest/MUSICIANS/`, etc. This matches the layout the recursive
    ///   sync produces, so `hvsc_root` stays the same.
    /// * Skips entries that already exist locally at the same size,
    ///   letting re-runs go fast even when the zip is unchanged.
    /// * Deletes the temp file when done (successful OR errored — a half-
    ///   downloaded zip is useless).
    ///
    /// Same event-stream shape as [`Self::start`], so the UI progress /
    /// polling code needs no changes.
    pub fn start_from_zip(url: &str, dest: &Path) -> Result<Self, String> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "URL must be http(s):// (got `{url}`). Paste a link to a \
                 HVSC C64Music zip archive."
            ));
        }
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("Cannot create destination {}: {e}", dest.display()))?;

        let (tx, rx) = bounded(PROGRESS_QUEUE_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = Arc::clone(&cancel);
        let dest = dest.to_path_buf();
        let url = url.trim().to_string();

        let join = thread::Builder::new()
            .name("hvsc-zip-sync".into())
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
                let result = rt.block_on(run_zip_sync(&url, dest, &tx, &cancel_for_thread));
                match result {
                    Ok(()) => {}
                    Err(e) => {
                        let _ = tx.send(HvscSyncEvent::Done(Err(e)));
                    }
                }
            })
            .map_err(|e| format!("Cannot spawn zip-sync thread: {e}"))?;

        Ok(Self {
            rx,
            cancel,
            join: Some(join),
        })
    }
}

impl Drop for HvscSyncHandle {
    fn drop(&mut self) {
        // Request cancellation, then DETACH the worker — do NOT join here.
        // The worker only checks `cancel` between requests, so an in-flight
        // `reqwest` (timeouts up to 180 s) would otherwise block whoever drops
        // this handle. On app close the App is dropped on the main thread, so a
        // blocking join would hang the whole app when the network is down.
        // Detaching is safe: the worker only writes files to disk and a partial
        // download is re-fetched on the next sync; on process exit the OS reaps
        // the thread.
        self.cancel.store(true, Ordering::SeqCst);
        // Drop the JoinHandle without joining -> the thread is detached.
        drop(self.join.take());
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
    // Inherit any user-configured proxy so HVSC sync works behind a
    // corporate firewall. None / empty → gosh-dl's default behaviour.
    let mut engine_config = EngineConfig {
        download_dir: dest.clone(),
        ..EngineConfig::default()
    };
    engine_config.http.proxy_url = crate::config::current_proxy_url();
    let engine = DownloadEngine::new(engine_config)
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
    let builder = reqwest::Client::builder()
        .user_agent("phosphor-hvsc-sync/0.4")
        .timeout(Duration::from_secs(30));
    let client = crate::config::apply_proxy(builder)
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

    let builder = reqwest::Client::builder()
        .user_agent("phosphor-hvsc-sync/0.4")
        .timeout(Duration::from_secs(180));
    let client = crate::config::apply_proxy(builder)
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

/// Download `url` into `<dest>/.phosphor-hvsc-download.archive`, then extract
/// its `C64Music/` subtree into `dest` (stripping the prefix).
///
/// Streaming rules:
/// * Download is chunked and cancellation-checked between chunks.
/// * Extraction is entry-by-entry and cancellation-checked before every
///   entry. We can't cancel *inside* a single entry's decompress — but
///   HVSC entries are tiny (median <2 KiB) so the granularity is fine.
///
/// The temp file is deleted on ALL exit paths (success, error, cancel).
async fn run_zip_sync(
    url: &str,
    dest: PathBuf,
    tx: &Sender<HvscSyncEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    use std::io::Write as _;

    // ── Phase 1: download to temp file ─────────────────────────────────────
    let tmp_path = dest.join(".phosphor-hvsc-download.archive");
    // Best-effort cleanup: remove any leftover from a previous crashed run.
    let _ = std::fs::remove_file(&tmp_path);

    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        current: format!("Connecting to {url}…"),
    });

    let builder = reqwest::Client::builder()
        .user_agent("phosphor-hvsc-zip-sync/0.4")
        // No overall timeout — a slow mirror serving 500 MB can legitimately
        // take an hour. Per-chunk progress in the loop below is what tells
        // the user we're alive.
        .connect_timeout(Duration::from_secs(30));
    let client = crate::config::apply_proxy(builder)
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let bytes_total = resp.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("Cannot create {}: {e}", tmp_path.display()))?;

    let mut stream = resp.bytes_stream();
    let mut bytes_done: u64 = 0;
    let mut last_report = std::time::Instant::now();

    use iced::futures::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            let _ = tx.send(HvscSyncEvent::Done(Err("Cancelled".to_string())));
            return Ok(());
        }
        let chunk = chunk.map_err(|e| format!("Download stream: {e}"))?;
        file.write_all(&chunk)
            .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
        bytes_done += chunk.len() as u64;
        // Throttle progress events to ~4/sec — the channel is bounded and
        // firing per-chunk saturates it under a fast connection.
        if last_report.elapsed() >= Duration::from_millis(250) {
            last_report = std::time::Instant::now();
            let mb_done = bytes_done / (1024 * 1024);
            let mb_total = bytes_total / (1024 * 1024);
            let current = if bytes_total > 0 {
                format!("Downloading archive… {mb_done} MB / {mb_total} MB")
            } else {
                format!("Downloading archive… {mb_done} MB")
            };
            let _ = tx.send(HvscSyncEvent::Progress {
                files_done: 0,
                files_total: 0,
                bytes_done,
                bytes_total,
                current,
            });
        }
    }
    file.flush()
        .map_err(|e| format!("flush {}: {e}", tmp_path.display()))?;
    drop(file);

    // ── Phase 2: sniff magic + extract ─────────────────────────────────────
    // Everything below is synchronous CPU/disk work. Keep it on this
    // worker thread — the tokio runtime is single-threaded but we're the
    // only task, so blocking is fine.
    let kind = detect_archive_kind(&tmp_path)?;
    let extract_result = match kind {
        ArchiveKind::Zip => extract_zip(&tmp_path, &dest, tx, cancel),
        ArchiveKind::SevenZ => extract_7z(&tmp_path, &dest, tx, cancel),
    };
    let _ = std::fs::remove_file(&tmp_path);

    let (extracted, skipped, corrupt) = extract_result?;

    let corrupt_note = if corrupt > 0 {
        format!(
            " — {corrupt} file(s) had bad CRCs and were skipped; re-run \
             `Sync HVSC now` to fetch fresh copies of those individually"
        )
    } else {
        String::new()
    };
    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: extracted,
        files_total: extracted,
        bytes_done,
        bytes_total,
        current: format!("Done. {extracted} new files, {skipped} already present{corrupt_note}."),
    });
    let _ = tx.send(HvscSyncEvent::Done(Ok(())));
    Ok(())
}

/// Extract every `C64Music/…` entry from `zip_path` into `dest`, stripping
/// the `C64Music/` prefix. Returns `(extracted, skipped_because_present,
/// corrupt)` where `corrupt` counts entries whose data failed a checksum
/// (CRC32 / entry-integrity) verification — those are dropped and the
/// caller reports the count.
fn extract_zip(
    zip_path: &Path,
    dest: &Path,
    tx: &Sender<HvscSyncEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(u32, u32, u32), String> {
    let file =
        std::fs::File::open(zip_path).map_err(|e| format!("open {}: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Not a valid zip archive: {e}"))?;
    let total = archive.len() as u32;

    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: 0,
        files_total: total,
        bytes_done: 0,
        bytes_total: 0,
        current: format!("Extracting… 0 / {total}"),
    });

    let mut extracted: u32 = 0;
    let mut skipped: u32 = 0;
    let mut corrupt: u32 = 0;
    let mut last_report = std::time::Instant::now();

    for i in 0..archive.len() {
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(HvscSyncEvent::Done(Err("Cancelled".to_string())));
            return Err("Cancelled".to_string());
        }
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Read zip entry #{i}: {e}"))?;

        // ZipArchive::by_index doesn't tell us the original file name
        // separately from enclosed_name (which sanitises it). Sanitise
        // first — an archive with `../` entries is user-facing input.
        let raw_name = match entry.enclosed_name() {
            Some(p) => p,
            None => {
                // Skip entries with suspicious paths (absolute, `..`, etc.).
                skipped += 1;
                continue;
            }
        };

        // Strip the top-level `C64Music/` prefix if present. HVSC's
        // official zips wrap everything in it; some third-party rebundles
        // don't. Handle both.
        let rel = strip_c64music_prefix(&raw_name);
        let Some(rel) = rel else {
            // Entry sits outside the C64Music/ subtree (readme at top of
            // archive, etc.) — drop silently.
            skipped += 1;
            continue;
        };

        let out_path = dest.join(&rel);
        if entry.is_dir() {
            // Just make sure the dir exists.
            if let Err(e) = std::fs::create_dir_all(&out_path) {
                return Err(format!("mkdir {}: {e}", out_path.display()));
            }
            continue;
        }

        // Skip-if-present: same size on disk → assume identical.
        // Not a checksum, but good enough for re-runs of an unchanged
        // zip. Users who want a fresh extract can delete `hvsc_root`
        // first.
        if let Ok(meta) = std::fs::metadata(&out_path) {
            if meta.len() == entry.size() {
                skipped += 1;
                extracted += 1; // count toward progress denominator
                continue;
            }
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .map_err(|e| format!("create {}: {e}", out_path.display()))?;
        match std::io::copy(&mut entry, &mut out) {
            Ok(_) => {
                extracted += 1;
            }
            Err(e) if is_checksum_error(&e) => {
                // Bad CRC on one entry inside an otherwise valid zip —
                // drop the partial file and press on. HVSC's file-by-file
                // rsync will backfill any missing entries on a later
                // update pass.
                drop(out);
                let _ = std::fs::remove_file(&out_path);
                eprintln!("[hvsc-sync/zip] CRC failed on {}", out_path.display());
                corrupt += 1;
            }
            Err(e) => {
                return Err(format!("extract {}: {e}", out_path.display()));
            }
        }

        if last_report.elapsed() >= Duration::from_millis(200) {
            last_report = std::time::Instant::now();
            let short = rel.to_string_lossy();
            let current = format!("Extracting… {extracted} / {total}  {short}");
            let _ = tx.send(HvscSyncEvent::Progress {
                files_done: extracted,
                files_total: total,
                bytes_done: 0,
                bytes_total: 0,
                current,
            });
        }
    }

    Ok((extracted, skipped, corrupt))
}

/// Container format we can extract in-app.
enum ArchiveKind {
    Zip,
    SevenZ,
}

/// Sniff the first bytes of `path` and decide which extractor to run.
/// URL suffix isn't consulted — mirrors sometimes serve one format via
/// a redirect from the other, and users paste opaque `download.php?…`
/// links. Magic bytes are the ground truth.
fn detect_archive_kind(path: &Path) -> Result<ArchiveKind, String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut buf = [0u8; 8];
    let n = f
        .read(&mut buf)
        .map_err(|e| format!("read magic bytes: {e}"))?;
    let head = &buf[..n];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Ok(ArchiveKind::Zip)
    } else if head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Ok(ArchiveKind::SevenZ)
    } else {
        Err(format!(
            "The download at that URL is not a recognised archive \
             (magic bytes: {:02X?}). Supported formats: .zip and .7z. \
             Did the URL return an HTML error page?",
            head
        ))
    }
}

/// Extract every `C64Music/…` entry from a 7z archive into `dest`, stripping
/// the `C64Music/` prefix. Same contract as [`extract_zip`].
///
/// 7z archives are usually solid-compressed, which means the internal
/// decoder emits entries in stream order and we can't cheaply seek.
/// [`SevenZReader::for_each_entries`] hides that — it walks entries and
/// hands us a decompressed `Read` per entry.
fn extract_7z(
    sz_path: &Path,
    dest: &Path,
    tx: &Sender<HvscSyncEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(u32, u32, u32), String> {
    let file =
        std::fs::File::open(sz_path).map_err(|e| format!("open {}: {e}", sz_path.display()))?;
    // sevenz-rust2's ArchiveReader::new takes (source, password). It reads
    // the archive length via Seek internally — no separate file-size arg
    // (unlike the abandoned sevenz-rust 0.6).
    let mut sz = sevenz_rust2::ArchiveReader::new(
        std::io::BufReader::new(file),
        sevenz_rust2::Password::empty(),
    )
    .map_err(|e| format!("Not a valid 7z archive: {e}"))?;

    // Total entries — sevenz-rust exposes them via the underlying archive.
    let total = sz.archive().files.len() as u32;
    let _ = tx.send(HvscSyncEvent::Progress {
        files_done: 0,
        files_total: total,
        bytes_done: 0,
        bytes_total: 0,
        current: format!("Extracting… 0 / {total}"),
    });

    let mut extracted: u32 = 0;
    let mut skipped: u32 = 0;
    let mut corrupt: u32 = 0;
    let mut last_report = std::time::Instant::now();
    let dest = dest.to_path_buf();
    let cancel = Arc::clone(cancel);
    let tx = tx.clone();

    // The closure signals continue/stop via the returned bool. To carry
    // rich errors out, stash them in a shared cell.
    let hard_err: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);

    let walk_result = sz.for_each_entries(|entry, reader| {
        if cancel.load(Ordering::SeqCst) {
            *hard_err.borrow_mut() = Some("Cancelled".to_string());
            return Ok(false);
        }

        // Sanitise: reject entries with `..` / absolute paths. Same
        // discipline as the zip extractor.
        let raw = Path::new(entry.name());
        let has_escape = raw.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        });
        if has_escape {
            skipped += 1;
            return Ok(true);
        }
        let Some(rel) = strip_c64music_prefix(raw) else {
            skipped += 1;
            return Ok(true);
        };
        let out_path = dest.join(&rel);

        if entry.is_directory() {
            if let Err(e) = std::fs::create_dir_all(&out_path) {
                *hard_err.borrow_mut() = Some(format!("mkdir {}: {e}", out_path.display()));
                return Ok(false);
            }
            return Ok(true);
        }

        // Skip-if-present at matching size.
        if let Ok(meta) = std::fs::metadata(&out_path) {
            if meta.len() == entry.size() {
                skipped += 1;
                extracted += 1; // count toward progress denominator
                return Ok(true);
            }
        }

        if let Some(parent) = out_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                *hard_err.borrow_mut() = Some(format!("mkdir {}: {e}", parent.display()));
                return Ok(false);
            }
        }
        let mut out = match std::fs::File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                *hard_err.borrow_mut() = Some(format!("create {}: {e}", out_path.display()));
                return Ok(false);
            }
        };
        match std::io::copy(reader, &mut out) {
            Ok(_) => {
                extracted += 1;
            }
            Err(e) if is_checksum_error(&e) => {
                // Per-file CRC failure. sevenz-rust2 emits this AFTER
                // fully draining the file's bounded reader, so the
                // shared block decoder is still positioned correctly
                // for the next entry — we can just log, delete the
                // partial file, and keep going.
                drop(out);
                let _ = std::fs::remove_file(&out_path);
                eprintln!("[hvsc-sync/7z] CRC failed on {}", out_path.display());
                corrupt += 1;
            }
            Err(e) => {
                *hard_err.borrow_mut() = Some(format!("extract {}: {e}", out_path.display()));
                return Ok(false);
            }
        }

        if last_report.elapsed() >= Duration::from_millis(200) {
            last_report = std::time::Instant::now();
            let short = rel.to_string_lossy();
            let _ = tx.send(HvscSyncEvent::Progress {
                files_done: extracted,
                files_total: total,
                bytes_done: 0,
                bytes_total: 0,
                current: format!("Extracting… {extracted} / {total}  {short}"),
            });
        }
        Ok(true)
    });

    if let Some(e) = hard_err.into_inner() {
        return Err(e);
    }
    walk_result.map_err(|e| friendly_7z_error(&e))?;
    Ok((extracted, skipped, corrupt))
}

/// True if this `io::Error` was raised because a decompressed entry failed
/// its integrity check.
///
/// Two paths surface it:
/// * `sevenz_rust2::Error::ChecksumVerificationFailed` wrapped by
///   `io::Error::other` inside the CRC-verifying reader.
/// * `zip::result::ZipError::Io(..)` / `Crc32` mismatches from the `zip`
///   crate, which set `ErrorKind::InvalidData` and include the words
///   "crc" / "checksum" in the message.
///
/// We match on the wrapped inner type first (accurate) and fall back to a
/// message contains-check for `zip` (the crate doesn't expose the CRC
/// variant as a strong type on this error path).
fn is_checksum_error(e: &std::io::Error) -> bool {
    if let Some(inner) = e.get_ref() {
        if let Some(sz) = inner.downcast_ref::<sevenz_rust2::Error>() {
            if matches!(sz, sevenz_rust2::Error::ChecksumVerificationFailed) {
                return true;
            }
        }
    }
    let msg = e.to_string().to_ascii_lowercase();
    if e.kind() == std::io::ErrorKind::InvalidData
        && (msg.contains("crc") || msg.contains("checksum"))
    {
        return true;
    }
    false
}

/// Translate raw `sevenz_rust2::Error` strings into actionable advice.
///
/// sevenz-rust2 supports LZMA, LZMA2, BCJ filters, and PPMD — that's every
/// method HVSC's various mirrors have shipped. If we hit an
/// `UnsupportedCompressionMethod` anyway, it's some exotic archive; point
/// the user at the .zip variant, which brona.dk always publishes alongside.
fn friendly_7z_error(e: &sevenz_rust2::Error) -> String {
    let raw = format!("{e}");
    if raw.contains("UnsupportedCompressionMethod") {
        format!(
            "This archive uses a 7z compression method we don't support \
             ({raw}). Try the .zip variant from the same mirror if there is one."
        )
    } else {
        format!("7z decode: {raw}")
    }
}

/// Given `C64Music/DEMOS/…/foo.sid`, return `Some("DEMOS/…/foo.sid")`.
/// Given `foo/bar.sid` with no top-level `C64Music`, return `None`.
fn strip_c64music_prefix(p: &Path) -> Option<PathBuf> {
    let mut comps = p.components();
    match comps.next()? {
        std::path::Component::Normal(top) if top.eq_ignore_ascii_case("C64Music") => {
            let rest: PathBuf = comps.collect();
            if rest.as_os_str().is_empty() {
                None
            } else {
                Some(rest)
            }
        }
        _ => None,
    }
}

/// Best-effort default URL for the "fast first-time sync" archive, derived
/// from the user's configured rsync mirror and the latest known HVSC
/// version. Returns `None` when the mirror host isn't one whose archive
/// URL pattern we know.
///
/// Known-good pattern (2020+): brona.dk publishes
/// `https://hvsc.brona.dk/HVSC/HVSC_<N>-all-of-them.7z` for every
/// release, and the STIL version in the tree matches that `<N>`.
///
/// `known_version` comes from `stil::check_hvsc_update` at startup, which is
/// what keeps this URL current; the constant below is only reached before
/// that answers or when the mirror is unreachable.
pub fn default_hvsc_zip_url(rsync_url: &str, known_version: Option<&str>) -> Option<String> {
    let host = url::Url::parse(rsync_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))?;
    // Strip an optional leading `v` from stored versions like "v85".
    let version_num: u32 = known_version
        .and_then(|s| {
            s.trim()
                .trim_start_matches(|c: char| c == 'v' || c == 'V')
                .parse()
                .ok()
        })
        .unwrap_or(LATEST_KNOWN_HVSC_VERSION);
    match host.as_str() {
        "hvsc.brona.dk" => Some(format!(
            "https://hvsc.brona.dk/HVSC/HVSC_{version_num}-all-of-them.7z"
        )),
        _ => None,
    }
}

/// Last-resort fallback only — a successful version check takes precedence.
/// HVSC ships ~every six months, so this drifts twice a year. Verified
/// 2026-08-22: #85 current, #86 not yet published. Bumping is optional; an
/// online install discovers the real number.
const LATEST_KNOWN_HVSC_VERSION: u32 = 85;

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
