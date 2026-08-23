// HVSC sync: download the collection as a single archive and extract it.
//
// This replaced a recursive HTTP crawl that walked the mirror's directory
// index and issued one GET per file (~75k of them). On community-run mirrors
// that ran for hours, and — worse — a run that stopped early left a tree that
// *looked* complete: every author directory created, most of them empty. One
// archive either arrives or it doesn't.
//
// Progress is streamed from the download and the extraction so the UI can
// show something during a ~500 MB transfer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

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
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Spawn a "fast first-time sync": download a single archive from `url`
    /// and extract it into `dest`.
    ///
    /// This is the only sync path. The per-file mirror crawl it replaced
    /// took hours and could leave a half-populated tree behind.
    ///
    /// Behaviour:
    /// * Downloads the archive to a temp file next to `dest` (same
    ///   filesystem, so no cross-device rename).
    /// * Extracts entries whose paths start with `C64Music/` — that prefix
    ///   is *stripped* so files land directly in `dest/DEMOS/`,
    ///   `dest/MUSICIANS/`, etc. — the layout `hvsc_root` expects, and the
    ///   same one the old per-file sync produced, so existing roots still work.
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
                // Bad CRC on one entry inside an otherwise valid archive —
                // drop the partial file and press on rather than failing the
                // whole extraction over one tune.
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
        //
        // 7z is SOLID: consecutive entries share one compressed block, so the
        // decoder only stays aligned if every entry's reader is read to the
        // end. Returning early without draining desynchronises it, and every
        // subsequent entry then decodes garbage, fails its CRC and is deleted
        // by the handler below — the sync reports success while writing
        // nothing. That made re-syncing unable to repair a partial tree: the
        // more files you already had, the more skips, the worse the damage.
        if let Ok(meta) = std::fs::metadata(&out_path) {
            if meta.len() == entry.size() {
                if let Err(e) = std::io::copy(reader, &mut std::io::sink()) {
                    *hard_err.borrow_mut() =
                        Some(format!("drain skipped entry {}: {e}", out_path.display()));
                    return Ok(false);
                }
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
    let first = comps.next()?;
    if let std::path::Component::Normal(top) = first {
        if top.eq_ignore_ascii_case("C64Music") {
            let rest: PathBuf = comps.collect();
            // The wrapper directory entry itself carries no file.
            return if rest.as_os_str().is_empty() {
                None
            } else {
                Some(rest)
            };
        }
    }
    // No wrapper: use the path as-is. Both callers previously treated `None`
    // as "skip this entry", so an archive that wasn't wrapped in `C64Music/`
    // had *every* entry silently dropped — the extraction reported success
    // and wrote nothing. That directly contradicted the callers' own comment
    // ("some third-party rebundles don't. Handle both.").
    Some(p.to_path_buf())
}

/// Best-effort default URL for the "fast first-time sync" archive, derived
/// from the user's configured mirror and the latest known HVSC
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
pub fn default_hvsc_zip_url(mirror_url: &str, known_version: Option<&str>) -> Option<String> {
    let host = url::Url::parse(mirror_url.trim())
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
mod solid_skip_tests {
    use super::*;

    /// Small solid 7z: `C64Music/MUSICIANS/t1..t12.sid`.
    const SAMPLE: &[u8] = include_bytes!("testdata/solid_sample.7z");

    fn extract_to(dir: &Path) -> Result<(u32, u32, u32), String> {
        let archive = dir.join("sample.7z");
        std::fs::write(&archive, SAMPLE).unwrap();
        let (tx, rx) = bounded(64);
        // Bounded channel: drain concurrently or `send` blocks once full.
        let drain = std::thread::spawn(move || rx.iter().count());
        let cancel = Arc::new(AtomicBool::new(false));
        let out = extract_7z(&archive, dir, &tx, &cancel);
        drop(tx);
        let _ = drain.join();
        std::fs::remove_file(&archive).ok();
        out
    }

    fn sid_count(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().map(|x| x == "sid").unwrap_or(false))
            .count()
    }

    #[test]
    fn extracts_everything_into_an_empty_dir() {
        let dir = std::env::temp_dir().join(format!("phos_solid_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (extracted, _skipped, corrupt) = extract_to(&dir).expect("extract");
        assert_eq!(corrupt, 0, "no CRC failures expected");
        assert_eq!(sid_count(&dir), 12, "all 12 tunes written");
        assert!(extracted >= 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Skipping an already-present file must not cost us any other file.
    ///
    /// CAVEAT, so nobody trusts this further than it goes: this fixture is
    /// built by libarchive, which does **not** emit solid blocks, so it does
    /// *not* reproduce the bug this guards against — verified by reverting the
    /// drain and watching this test still pass. The real failure needs a solid
    /// archive (HVSC's own), where a skipped entry left the shared block
    /// decoder misaligned and every later entry failed its CRC and was
    /// deleted while the sync reported success. That path was verified
    /// end-to-end against HVSC_85-all-of-them.7z: 35487 CRC failures and 0
    /// files written before the fix, 0 failures and 61157 files after.
    /// Making this reproduce in-process needs a solid fixture, which needs
    /// sevenz-rust2's `compress` feature.
    #[test]
    fn skipping_an_existing_file_does_not_corrupt_the_rest() {
        let dir = std::env::temp_dir().join(format!("phos_solid_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Populate the tree, then delete all but one file. The survivor is
        // skipped on the next pass; everything after it must still extract.
        extract_to(&dir).expect("seed");
        let mus = dir.join("MUSICIANS");
        for e in std::fs::read_dir(&mus).unwrap().filter_map(Result::ok) {
            if e.file_name() != std::ffi::OsString::from("t1.sid") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        assert_eq!(sid_count(&dir), 1, "exactly one file left to be skipped");

        let (_extracted, skipped, corrupt) = extract_to(&dir).expect("re-extract");
        assert!(skipped >= 1, "the surviving file should be skipped");
        assert_eq!(corrupt, 0, "a skip must not desync the solid-block decoder");
        assert_eq!(sid_count(&dir), 12, "every tune restored after the skip");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    #[test]
    fn strips_the_wrapper_when_present() {
        assert_eq!(
            strip_c64music_prefix(Path::new("C64Music/MUSICIANS/H/Hubbard_Rob/Commando.sid")),
            Some(PathBuf::from("MUSICIANS/H/Hubbard_Rob/Commando.sid"))
        );
        // Case-insensitive, as HVSC rebundles vary.
        assert_eq!(
            strip_c64music_prefix(Path::new("c64music/DEMOS/x.sid")),
            Some(PathBuf::from("DEMOS/x.sid"))
        );
        // The wrapper directory entry itself has nothing under it.
        assert_eq!(strip_c64music_prefix(Path::new("C64Music")), None);
    }

    #[test]
    fn keeps_unwrapped_paths_instead_of_dropping_them() {
        // Regression: this used to return None, and both extractors treat
        // None as "skip". An archive without the wrapper therefore extracted
        // zero files while reporting success.
        assert_eq!(
            strip_c64music_prefix(Path::new("MUSICIANS/H/Hubbard_Rob/Commando.sid")),
            Some(PathBuf::from("MUSICIANS/H/Hubbard_Rob/Commando.sid"))
        );
        assert_eq!(
            strip_c64music_prefix(Path::new("readme.1st")),
            Some(PathBuf::from("readme.1st"))
        );
    }
}
