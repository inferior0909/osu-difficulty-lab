use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    Proxy, StatusCode,
    blocking::Client,
    header::{COOKIE, RANGE},
};
use serde::Deserialize;
use sevenz_rust::{Error as SevenZError, decompress_file_with_extract_fn};
use zip::ZipArchive;

use crate::{Analyzer, FeatureStore};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackImportReport {
    pub processed: usize,
    pub inserted: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownloadProgress {
    pub attempt: u8,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackDownloadSource {
    Official,
    Hinamizawa,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackDownloadReport {
    pub pack_id: String,
    pub succeeded: bool,
    pub already_complete: bool,
    pub skipped: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PackDownloadEvent {
    Started {
        pack_id: String,
    },
    Progress {
        pack_id: String,
        progress: DownloadProgress,
    },
    Finished(PackDownloadReport),
}

const MAX_DOWNLOAD_ATTEMPTS: u8 = 3;
const MIN_DOWNLOAD_BYTES_PER_SECOND: f64 = 1024.0 * 1024.0;
const SLOW_DOWNLOAD_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
#[error(
    "download rate {bytes_per_second:.2} MiB/s stayed below 1.00 MiB/s for {window_seconds} seconds; reconnecting"
)]
struct SlowDownloadError {
    bytes_per_second: f64,
    window_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct HinamizawaCascadeResponse {
    success: bool,
    download_url: String,
}

pub struct PackImporter {
    client: Client,
}
impl PackImporter {
    pub fn new(cookie_file: Option<&Path>) -> Result<Self> {
        Self::with_proxy(cookie_file, None)
    }

    /// Build an importer whose HTTP requests are routed through `proxy_url`.
    /// HTTP(S) and SOCKS5 proxy URLs are supported by reqwest.
    pub fn with_proxy(cookie_file: Option<&Path>, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = Client::builder()
            .user_agent(
                "osu-difficulty-lab/0.1 (+https://github.com/osuplusplus/osu-difficulty-lab)",
            )
            // Large official packs can legitimately take more than three minutes.
            // Keep a finite deadline for stalled requests without forcing healthy
            // large transfers to resume every 180 seconds.
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(15 * 60));
        if let Some(proxy_url) = proxy_url {
            builder =
                builder.proxy(Proxy::all(proxy_url).map_err(|_| anyhow!("invalid proxy URL"))?);
        }
        let client = builder.build()?;
        let _ = cookie_file; // Cookies are attached per request; never stored by this crate.
        Ok(Self { client })
    }

    pub fn validate_cookie_file(&self, cookie_file: &Path) -> Result<()> {
        let _ = &self.client;
        read_netscape_cookie(cookie_file).map(|_| ())
    }

    /// Download unfinished packs concurrently. Database writes deliberately remain on
    /// the calling thread: SQLite has a single writer, while HTTP transfers do not.
    /// Archives stay in `tmp/` until `ingest-downloaded` imports them and removes them.
    pub fn download_packs_concurrently(
        &self,
        store: &FeatureStore,
        pack_ids: &[String],
        cookie_file: &Path,
        concurrency: usize,
    ) -> Result<Vec<PackDownloadReport>> {
        self.download_packs_concurrently_with_progress(
            store,
            pack_ids,
            cookie_file,
            concurrency,
            |_| {},
        )
    }

    pub fn download_packs_concurrently_with_progress<F>(
        &self,
        store: &FeatureStore,
        pack_ids: &[String],
        cookie_file: &Path,
        concurrency: usize,
        on_event: F,
    ) -> Result<Vec<PackDownloadReport>>
    where
        F: Fn(PackDownloadEvent) + Sync,
    {
        let cookie = read_netscape_cookie(cookie_file)?;
        let mut queued = VecDeque::new();
        let mut reports = Vec::new();
        for pack_id in pack_ids {
            if store.pack_is_complete(pack_id)? {
                let report = PackDownloadReport {
                    pack_id: pack_id.clone(),
                    succeeded: false,
                    already_complete: true,
                    skipped: false,
                    error: None,
                };
                on_event(PackDownloadEvent::Finished(report.clone()));
                reports.push(report);
            } else if store.pack_is_excluded(pack_id)? {
                let report = PackDownloadReport {
                    pack_id: pack_id.clone(),
                    succeeded: false,
                    already_complete: false,
                    skipped: true,
                    error: None,
                };
                on_event(PackDownloadEvent::Finished(report.clone()));
                reports.push(report);
            } else {
                store.mark_pack(pack_id, "pending", "queued", None)?;
                queued.push_back(pack_id.clone());
            }
        }
        if queued.is_empty() {
            return Ok(reports);
        }

        let queue = Arc::new(Mutex::new(queued));
        let results = Arc::new(Mutex::new(Vec::new()));
        let root = store.root().to_path_buf();
        let workers = concurrency.clamp(1, pack_ids.len());

        thread::scope(|scope| {
            for _ in 0..workers {
                let queue = Arc::clone(&queue);
                let results = Arc::clone(&results);
                let root = root.clone();
                let cookie = cookie.clone();
                // Clones share reqwest's connection pool, avoiding a new TLS
                // connection for every pack handled by a worker.
                let client = self.client.clone();
                let on_event = &on_event;
                scope.spawn(move || {
                    let importer = PackImporter { client };
                    loop {
                        let Some(pack_id) =
                            queue.lock().expect("download queue poisoned").pop_front()
                        else {
                            break;
                        };
                        on_event(PackDownloadEvent::Started {
                            pack_id: pack_id.clone(),
                        });
                        let result = (|| -> Result<bool> {
                            if is_known_nonstandard_pack_id(&pack_id) {
                                return Ok(false);
                            }
                            let destination = root.join("tmp").join(format!("{pack_id}.part"));
                            importer.download_pack_to_file(
                                &pack_id,
                                &cookie,
                                &destination,
                                |progress| {
                                    on_event(PackDownloadEvent::Progress {
                                        pack_id: pack_id.clone(),
                                        progress,
                                    });
                                },
                            )?;
                            Ok(true)
                        })();
                        let report = match result {
                            Ok(true) => PackDownloadReport {
                                pack_id,
                                succeeded: true,
                                already_complete: false,
                                skipped: false,
                                error: None,
                            },
                            Ok(false) => PackDownloadReport {
                                pack_id,
                                succeeded: false,
                                already_complete: false,
                                skipped: true,
                                error: None,
                            },
                            Err(error) => PackDownloadReport {
                                pack_id,
                                succeeded: false,
                                already_complete: false,
                                skipped: false,
                                error: Some(error.to_string()),
                            },
                        };
                        on_event(PackDownloadEvent::Finished(report.clone()));
                        results
                            .lock()
                            .expect("download result list poisoned")
                            .push(report);
                    }
                });
            }
        });

        reports.extend(
            Arc::try_unwrap(results)
                .expect("download workers still hold result list")
                .into_inner()
                .expect("download result list poisoned"),
        );
        reports.sort_by(|left, right| left.pack_id.cmp(&right.pack_id));
        for report in &reports {
            if report.already_complete {
                continue;
            } else if report.succeeded {
                let source = root
                    .join("tmp")
                    .join(format!("{}.part", report.pack_id))
                    .to_string_lossy()
                    .into_owned();
                store.mark_pack(&report.pack_id, &source, "downloaded", None)?;
            } else if report.skipped {
                store.mark_pack(
                    &report.pack_id,
                    "catalogue-filter",
                    "excluded",
                    Some("pack contains no osu!standard beatmaps"),
                )?;
            } else {
                store.mark_pack(
                    &report.pack_id,
                    "pending",
                    "failed",
                    report.error.as_deref(),
                )?;
            }
        }
        Ok(reports)
    }

    /// Fetch all official pack catalogue pages. The caller can persist or review the returned IDs before downloading.
    pub fn sync_catalog(&self, pack_types: &[&str]) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for pack_type in pack_types {
            for page in 1..=64 {
                let url = format!("https://osu.ppy.sh/beatmaps/packs?type={pack_type}&page={page}");
                let body = self.client.get(&url).send()?.error_for_status()?.text()?;
                let page_ids = extract_pack_ids(&body);
                if page_ids.is_empty() {
                    break;
                }
                ids.extend(page_ids);
                if !body.contains("pagination-v2__link pagination-v2__link--quick") {
                    break;
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Return only official osu!standard packs. Their canonical IDs are `S` followed
    /// by digits (for example `S1845`); `ST`, `SM`, and `SC` are other game modes.
    pub fn sync_standard_catalog(&self, pack_types: &[&str]) -> Result<Vec<String>> {
        Ok(self
            .sync_catalog(pack_types)?
            .into_iter()
            .filter(|id| is_standard_pack_id(id))
            .collect())
    }

    /// Download exactly one authenticated official pack, import it transactionally, and remove its temporary bytes only on success.
    pub fn download_and_import(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        pack_id: &str,
        cookie_file: &Path,
    ) -> Result<PackImportReport> {
        self.download_and_import_with_progress(store, analyzer, pack_id, cookie_file, |_| {})
    }

    pub fn download_and_import_with_progress<F>(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        pack_id: &str,
        cookie_file: &Path,
        on_progress: F,
    ) -> Result<PackImportReport>
    where
        F: FnMut(DownloadProgress),
    {
        self.download_and_import_from_source_with_progress(
            store,
            analyzer,
            pack_id,
            cookie_file,
            PackDownloadSource::Official,
            on_progress,
        )
    }

    pub fn download_and_import_from_source_with_progress<F>(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        pack_id: &str,
        cookie_file: &Path,
        source: PackDownloadSource,
        mut on_progress: F,
    ) -> Result<PackImportReport>
    where
        F: FnMut(DownloadProgress),
    {
        if store.pack_is_complete(pack_id)? || store.pack_is_excluded(pack_id)? {
            return Ok(PackImportReport::default());
        }
        if is_known_nonstandard_pack_id(pack_id) {
            store.mark_pack(
                pack_id,
                "catalogue-filter",
                "excluded",
                Some("pack contains no osu!standard beatmaps"),
            )?;
            return Ok(PackImportReport::default());
        }
        let temporary = store.root().join("tmp").join(format!("{pack_id}.part"));
        if temporary.is_file() {
            match self.import_downloaded_pack(store, analyzer, pack_id) {
                Ok(report) => return Ok(report),
                Err(error) if is_rar_archive(&temporary)? => {
                    let error = format!("RAR archive support is unavailable: {error}");
                    store.mark_pack(
                        pack_id,
                        &temporary.to_string_lossy(),
                        "failed",
                        Some(&error),
                    )?;
                    bail!("official pack {pack_id}: {error}");
                }
                Err(_) => {
                    // The archive may be an incomplete download. Re-download below;
                    // `download_to_file` resumes where possible.
                }
            }
        }
        let cookie = read_netscape_cookie(cookie_file)?;
        let pack_page = format!("https://osu.ppy.sh/beatmaps/packs/{pack_id}");
        let mut final_error = String::new();
        for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
            let result = (|| -> Result<PackImportReport> {
                let url = self.resolve_pack_download_url_from_source(pack_id, &cookie, source)?;
                store.mark_pack(pack_id, &url, "downloading", None)?;
                self.download_to_file(&url, &cookie, &temporary, attempt, &mut on_progress)?;
                let report = self.import_archive(store, analyzer, &temporary)?;
                if report.failed > 0 {
                    bail!(
                        "{} of {} osu!standard beatmaps failed analysis; retained archive for retry",
                        report.failed,
                        report.processed
                    );
                }
                fs::remove_file(&temporary)?;
                store.mark_pack(pack_id, &url, "complete", None)?;
                Ok(report)
            })();
            match result {
                Ok(report) => return Ok(report),
                Err(error) => {
                    let reconnect_immediately = error.downcast_ref::<SlowDownloadError>().is_some();
                    final_error = error.to_string();
                    let status = if attempt == MAX_DOWNLOAD_ATTEMPTS {
                        "failed"
                    } else {
                        "retrying"
                    };
                    store.mark_pack(pack_id, &pack_page, status, Some(&final_error))?;
                    if attempt < MAX_DOWNLOAD_ATTEMPTS && !reconnect_immediately {
                        std::thread::sleep(Duration::from_secs(2_u64.pow(attempt as u32)));
                    }
                }
            }
        }
        bail!(
            "official pack {pack_id} failed after {MAX_DOWNLOAD_ATTEMPTS} attempts: {final_error}"
        )
    }

    /// Import an archive previously staged by `download-packs`, without making any
    /// network request. Successful imports retain only individual `.osu` sources.
    pub fn import_downloaded_pack(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        pack_id: &str,
    ) -> Result<PackImportReport> {
        if store.pack_is_complete(pack_id)? || store.pack_is_excluded(pack_id)? {
            return Ok(PackImportReport::default());
        }
        let temporary = store.root().join("tmp").join(format!("{pack_id}.part"));
        if !temporary.is_file() {
            bail!("no downloaded archive exists for official pack {pack_id}");
        }
        let source = temporary.to_string_lossy().into_owned();
        store.mark_pack(pack_id, &source, "processing", None)?;
        match self.import_archive(store, analyzer, &temporary) {
            Ok(report) if report.failed == 0 => {
                fs::remove_file(&temporary)?;
                store.mark_pack(pack_id, "local-staged", "complete", None)?;
                Ok(report)
            }
            Ok(report) => {
                let error = format!(
                    "{} of {} osu!standard beatmaps failed analysis; retained archive for retry",
                    report.failed, report.processed
                );
                store.mark_pack(pack_id, &source, "failed", Some(&error))?;
                bail!("official pack {pack_id}: {error}");
            }
            Err(error) => {
                store.mark_pack(pack_id, &source, "failed", Some(&error.to_string()))?;
                Err(error)
            }
        }
    }

    fn download_to_file<F>(
        &self,
        url: &str,
        cookie: &str,
        destination: &Path,
        attempt: u8,
        on_progress: &mut F,
    ) -> Result<()>
    where
        F: FnMut(DownloadProgress),
    {
        let existing_bytes = destination
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut request = self.client.get(url).header(COOKIE, cookie);
        if existing_bytes > 0 {
            request = request.header(RANGE, format!("bytes={existing_bytes}-"));
        }
        let mut response = request.send()?;
        let is_resumed = if existing_bytes > 0 && response.status() == StatusCode::PARTIAL_CONTENT {
            true
        } else if existing_bytes > 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            // The retained archive is already complete (or the server no longer
            // accepts its range). Start a clean request instead of retrying 416.
            File::create(destination)?;
            response = self.client.get(url).header(COOKIE, cookie).send()?;
            false
        } else {
            false
        };
        let mut response = response.error_for_status()?;
        let initial_bytes = if is_resumed { existing_bytes } else { 0 };
        let total_bytes = response
            .content_length()
            .map(|length| length + initial_bytes);
        let started = Instant::now();
        let mut last_report = started;
        let mut rate_window_started = started;
        let mut rate_window_bytes = 0_u64;
        let mut downloaded_bytes = initial_bytes;
        let mut buffer = [0_u8; 128 * 1024];
        let mut file = if is_resumed {
            OpenOptions::new().append(true).open(destination)?
        } else {
            File::create(destination)?
        };
        loop {
            let count = response.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])?;
            downloaded_bytes += count as u64;
            rate_window_bytes += count as u64;
            let now = Instant::now();
            let rate_window_elapsed = now.duration_since(rate_window_started);
            if rate_window_elapsed >= SLOW_DOWNLOAD_WINDOW {
                let rate = rate_window_bytes as f64 / rate_window_elapsed.as_secs_f64();
                if is_below_minimum_download_rate(rate_window_elapsed, rate_window_bytes) {
                    return Err(SlowDownloadError {
                        bytes_per_second: rate / 1024.0 / 1024.0,
                        window_seconds: SLOW_DOWNLOAD_WINDOW.as_secs(),
                    }
                    .into());
                }
                rate_window_started = now;
                rate_window_bytes = 0;
            }
            if now.duration_since(last_report) >= Duration::from_millis(250) {
                on_progress(DownloadProgress {
                    attempt,
                    downloaded_bytes,
                    total_bytes,
                    bytes_per_second: (downloaded_bytes - initial_bytes) as f64
                        / now.duration_since(started).as_secs_f64().max(f64::EPSILON),
                });
                last_report = now;
            }
        }
        // The verified `.part` file is resumable after a crash, so forcing all
        // concurrent archives through a physical-disk sync only adds I/O stalls.
        file.flush()?;
        if let Some(expected_bytes) = total_bytes
            && downloaded_bytes != expected_bytes
        {
            bail!(
                "download ended at {downloaded_bytes} bytes, but the server declared {expected_bytes} bytes"
            );
        }
        let elapsed = started.elapsed();
        on_progress(DownloadProgress {
            attempt,
            downloaded_bytes,
            total_bytes,
            bytes_per_second: (downloaded_bytes - initial_bytes) as f64
                / elapsed.as_secs_f64().max(f64::EPSILON),
        });
        Ok(())
    }

    fn download_pack_to_file<F>(
        &self,
        pack_id: &str,
        cookie: &str,
        destination: &Path,
        mut on_progress: F,
    ) -> Result<()>
    where
        F: FnMut(DownloadProgress),
    {
        let mut final_error = String::new();
        for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
            let result = (|| -> Result<()> {
                let url = self.resolve_pack_download_url(pack_id, cookie)?;
                self.download_to_file(&url, cookie, destination, attempt, &mut on_progress)?;
                verify_archive_complete(destination)
            })();
            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let reconnect_immediately = error.downcast_ref::<SlowDownloadError>().is_some();
                    final_error = error.to_string();
                    if attempt < MAX_DOWNLOAD_ATTEMPTS && !reconnect_immediately {
                        thread::sleep(Duration::from_secs(2_u64.pow(attempt as u32)));
                    }
                }
            }
        }
        bail!(
            "official pack {pack_id} failed after {MAX_DOWNLOAD_ATTEMPTS} attempts: {final_error}"
        )
    }

    pub fn import_archive(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        archive_path: &Path,
    ) -> Result<PackImportReport> {
        let mut signature = [0_u8; 6];
        File::open(archive_path)?.read_exact(&mut signature)?;
        if signature == *b"7z\xBC\xAF\x27\x1C" {
            return self.import_7z(store, analyzer, archive_path);
        }
        if signature == *b"Rar!\x1A\x07" {
            bail!("RAR archive support is unavailable");
        }

        let file = File::open(archive_path)
            .with_context(|| format!("open archive {}", archive_path.display()))?;
        let mut archive = ZipArchive::new(file)?;
        let mut report = PackImportReport::default();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let name = entry.name().to_ascii_lowercase();
            if name.ends_with(".osu") {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                self.import_beatmap(store, analyzer, &bytes, &mut report);
            } else if name.ends_with(".osz") {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                self.import_osz(store, analyzer, &bytes, &mut report)?;
            }
        }
        if report.processed == 0 {
            bail!("official pack archive contains no .osu beatmaps");
        }
        Ok(report)
    }

    fn import_7z(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        archive_path: &Path,
    ) -> Result<PackImportReport> {
        let mut report = PackImportReport::default();
        let scratch = store.root().join("tmp").join("7z-scratch");
        let extraction = decompress_file_with_extract_fn(
            archive_path,
            &scratch,
            |entry, reader, _destination| {
                let name = entry.name().to_ascii_lowercase();
                if entry.is_directory() {
                    return Ok(true);
                }
                if name.ends_with(".osu") || name.ends_with(".osz") {
                    let mut bytes = Vec::with_capacity(entry.size() as usize);
                    reader.read_to_end(&mut bytes).map_err(SevenZError::io)?;
                    if name.ends_with(".osu") {
                        self.import_beatmap(store, analyzer, &bytes, &mut report);
                    } else if self
                        .import_osz(store, analyzer, &bytes, &mut report)
                        .is_err()
                    {
                        report.failed += 1;
                    }
                } else {
                    std::io::copy(reader, &mut std::io::sink()).map_err(SevenZError::io)?;
                }
                Ok(true)
            },
        );
        if scratch.exists() {
            fs::remove_dir_all(&scratch)
                .with_context(|| format!("remove 7z scratch directory {}", scratch.display()))?;
        }
        extraction.with_context(|| format!("extract 7z archive {}", archive_path.display()))?;
        if report.processed == 0 {
            bail!("official 7z pack archive contains no .osu beatmaps");
        }
        Ok(report)
    }

    fn resolve_pack_download_url(&self, pack_id: &str, cookie: &str) -> Result<String> {
        let detail_url = format!("https://osu.ppy.sh/beatmaps/packs/{pack_id}");
        let page = self
            .client
            .get(&detail_url)
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?
            .text()?;
        extract_download_url(&page).with_context(|| {
            format!(
                "official pack {pack_id} did not expose an authenticated download link; refresh the osu.ppy.sh Cookie file"
            )
        })
    }

    fn resolve_pack_download_url_from_source(
        &self,
        pack_id: &str,
        cookie: &str,
        source: PackDownloadSource,
    ) -> Result<String> {
        match source {
            PackDownloadSource::Official => self.resolve_pack_download_url(pack_id, cookie),
            PackDownloadSource::Hinamizawa => {
                match self.resolve_hinamizawa_pack_download_url(pack_id) {
                    Ok(url) => Ok(url),
                    Err(mirror_error) => self.resolve_pack_download_url(pack_id, cookie).with_context(|| {
                        format!(
                            "Hinamizawa could not resolve official pack {pack_id}: {mirror_error}; official fallback also failed"
                        )
                    }),
                }
            }
        }
    }

    fn resolve_hinamizawa_pack_download_url(&self, pack_id: &str) -> Result<String> {
        let url =
            format!("https://mirror.hinamizawa.ai/v3/osu/packs/{pack_id}/cascade?format=json");
        let body = self.client.get(&url).send()?.error_for_status()?.text()?;
        extract_hinamizawa_cascade_url(&body)
            .with_context(|| format!("parse Hinamizawa cascade response for {pack_id}"))
    }
    fn import_osz(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        bytes: &[u8],
        report: &mut PackImportReport,
    ) -> Result<()> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if entry.name().to_ascii_lowercase().ends_with(".osu") {
                let mut map = Vec::new();
                entry.read_to_end(&mut map)?;
                self.import_beatmap(store, analyzer, &map, report);
            }
        }
        Ok(())
    }
    fn import_beatmap(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        bytes: &[u8],
        report: &mut PackImportReport,
    ) {
        report.processed += 1;
        // Official catalogues contain packs for all four osu! modes. This data set is
        // deliberately osu!standard-only, so a non-standard `.osu` is an expected
        // exclusion rather than a failed import.
        if !is_standard_beatmap(bytes) {
            report.skipped += 1;
            return;
        }
        match analyzer
            .analyze_bytes(bytes)
            .and_then(|(metadata, record)| {
                store.persist_beatmap_source(record.beatmap_id, bytes)?;
                store
                    .append_raw(&metadata, &record)
                    .map(|inserted| (metadata, inserted))
            }) {
            Ok((_, true)) => report.inserted += 1,
            Ok((_, false)) => report.skipped += 1,
            Err(_) => report.failed += 1,
        }
    }
}

fn is_standard_beatmap(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        // Let the analyzer report malformed or legacy-encoded standard maps as real
        // failures. We only skip a map when its mode can be established safely.
        return true;
    };
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("Mode")
        {
            return value.trim().parse::<u8>().map_or(true, |mode| mode == 0);
        }
    }
    // `Mode` is optional in old osu! files and defaults to osu!standard.
    true
}

fn is_rar_archive(path: &Path) -> Result<bool> {
    let mut signature = [0_u8; 6];
    File::open(path)?.read_exact(&mut signature)?;
    Ok(signature == *b"Rar!\x1A\x07")
}

fn extract_pack_ids(body: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let marker = "/beatmaps/packs/";
    let mut rest = body;
    while let Some(index) = rest.find(marker) {
        rest = &rest[index + marker.len()..];
        let id = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>();
        if id.len() > 1 && id.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            ids.push(id);
        }
    }
    ids
}

fn is_standard_pack_id(pack_id: &str) -> bool {
    pack_id.strip_prefix('S').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn is_known_nonstandard_pack_id(pack_id: &str) -> bool {
    ["ST", "SM", "SC"].into_iter().any(|prefix| {
        pack_id.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn extract_download_url(body: &str) -> Option<String> {
    body.split("href=\"")
        .skip(1)
        .filter_map(|fragment| fragment.split_once('"').map(|(url, _)| url))
        // Pack names are part of the URL. Decode every HTML entity in the
        // attribute, not only `&amp;`: for example, A20 contains `&#039;`.
        .map(decode_html_entities)
        .find(|url| {
            url.starts_with("https://packs.ppy.sh/") || url.starts_with("https://dl.osu.ppy.sh/")
        })
}

fn extract_hinamizawa_cascade_url(body: &str) -> Result<String> {
    let response: HinamizawaCascadeResponse = serde_json::from_str(body)?;
    if !response.success {
        bail!("Hinamizawa reported an unsuccessful pack cascade")
    }
    if !response.download_url.starts_with("https://") {
        bail!("Hinamizawa returned a non-HTTPS download URL")
    }
    Ok(response.download_url)
}

fn decode_html_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut remainder = value;

    while let Some(entity_start) = remainder.find('&') {
        decoded.push_str(&remainder[..entity_start]);
        let after_ampersand = &remainder[entity_start + 1..];
        let Some(entity_end) = after_ampersand.find(';') else {
            decoded.push('&');
            decoded.push_str(after_ampersand);
            return decoded;
        };

        let entity = &after_ampersand[..entity_end];
        let character = match entity {
            "amp" => Some('&'),
            "apos" | "#039" => Some('\''),
            "quot" => Some('\"'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|number| u32::from_str_radix(number, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|number| number.parse::<u32>().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = character {
            decoded.push(character);
        } else {
            decoded.push('&');
            decoded.push_str(entity);
            decoded.push(';');
        }
        remainder = &after_ampersand[entity_end + 1..];
    }

    decoded.push_str(remainder);
    decoded
}

fn read_netscape_cookie(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path).context("read cookie file")?;
    let values = content
        .lines()
        // Netscape uses `#HttpOnly_` as a data-line prefix, not a comment.
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') || trimmed.starts_with("#HttpOnly_")
        })
        .filter_map(|line| {
            let parts = line.split('\t').collect::<Vec<_>>();
            let domain = parts
                .first()
                .map(|value| value.trim_start_matches("#HttpOnly_"))?;
            (parts.len() >= 7 && (domain == "ppy.sh" || domain.ends_with(".ppy.sh")))
                .then(|| format!("{}={}", parts[5], parts[6]))
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        bail!(
            "cookie file contains no Netscape-format osu.ppy.sh cookies; export cookies.txt again from the logged-in osu.ppy.sh tab"
        );
    }
    Ok(values.join("; "))
}

fn is_below_minimum_download_rate(window: Duration, transferred_bytes: u64) -> bool {
    transferred_bytes as f64 / window.as_secs_f64().max(f64::EPSILON)
        < MIN_DOWNLOAD_BYTES_PER_SECOND
}

fn verify_archive_complete(archive_path: &Path) -> Result<()> {
    let mut signature = [0_u8; 6];
    File::open(archive_path)
        .with_context(|| format!("open downloaded archive {}", archive_path.display()))?
        .read_exact(&mut signature)
        .with_context(|| {
            format!(
                "read downloaded archive signature {}",
                archive_path.display()
            )
        })?;

    if signature == *b"7z\xBC\xAF\x27\x1C" {
        let scratch = archive_path.with_extension("verify");
        let validation =
            decompress_file_with_extract_fn(archive_path, &scratch, |_entry, reader, _| {
                std::io::copy(reader, &mut std::io::sink()).map_err(SevenZError::io)?;
                Ok(true)
            });
        if scratch.exists() {
            fs::remove_dir_all(&scratch)
                .with_context(|| format!("remove 7z validation directory {}", scratch.display()))?;
        }
        return validation.with_context(|| format!("verify 7z archive {}", archive_path.display()));
    }
    if signature == *b"Rar!\x1A\x07" {
        bail!("RAR archive support is unavailable");
    }

    let file = File::open(archive_path)
        .with_context(|| format!("open downloaded archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("open ZIP archive {}", archive_path.display()))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        std::io::copy(&mut entry, &mut std::io::sink())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        PackDownloadEvent, PackImporter, extract_download_url, extract_hinamizawa_cascade_url,
        is_known_nonstandard_pack_id, is_standard_beatmap, verify_archive_complete,
    };
    use crate::{Analyzer, AnalyzerConfig, FeatureStore};
    use std::{io::Write, sync::Mutex, time::Duration};

    #[test]
    fn extracts_authenticated_pack_download() {
        let html = r#"<a href="https://packs.ppy.sh/S1%20-%20test.zip?sig=one&amp;expires=two">download</a>"#;
        assert_eq!(
            extract_download_url(html).as_deref(),
            Some("https://packs.ppy.sh/S1%20-%20test.zip?sig=one&expires=two")
        );
    }

    #[test]
    fn accepts_http_and_socks_proxy_urls_without_connecting() {
        PackImporter::with_proxy(None, Some("http://127.0.0.1:7890")).unwrap();
        PackImporter::with_proxy(None, Some("socks5://127.0.0.1:1080")).unwrap();
        assert!(PackImporter::with_proxy(None, Some("not a URL")).is_err());
    }

    #[test]
    fn decodes_numeric_entities_in_pack_filename() {
        let html =
            r#"<a href="https://packs.ppy.sh/A20%20-%20ALiCE&#039;S%20EMOTiON">download</a>"#;
        assert_eq!(
            extract_download_url(html).as_deref(),
            Some("https://packs.ppy.sh/A20%20-%20ALiCE'S%20EMOTiON")
        );
    }

    #[test]
    fn extracts_hinamizawa_cascade_download_url() {
        let body = r#"{
            "download_url":"https://packs.ppy.sh/A26%20-%20Pack.zip",
            "latency_ms":313,
            "layer":1,
            "source":"osu_cdn",
            "success":true,
            "tag":"A26"
        }"#;
        assert_eq!(
            extract_hinamizawa_cascade_url(body).unwrap(),
            "https://packs.ppy.sh/A26%20-%20Pack.zip"
        );
    }

    #[test]
    fn rejects_unsuccessful_hinamizawa_cascade_response() {
        let body = r#"{"download_url":"https://example.com/pack.zip","success":false}"#;
        assert!(extract_hinamizawa_cascade_url(body).is_err());
    }

    #[test]
    fn accepts_httponly_netscape_cookie_lines() {
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            path.path(),
            "# Netscape HTTP Cookie File\n#HttpOnly_.osu.ppy.sh\tTRUE\t/\tTRUE\t0\tosu_session\tsecret\n",
        )
        .unwrap();
        assert_eq!(
            super::read_netscape_cookie(path.path()).unwrap(),
            "osu_session=secret"
        );
    }

    #[test]
    fn accepts_parent_ppy_domain_cookie_lines() {
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            path.path(),
            ".ppy.sh\tTRUE\t/\tTRUE\t0\tosu_session\tsecret\n",
        )
        .unwrap();
        assert_eq!(
            super::read_netscape_cookie(path.path()).unwrap(),
            "osu_session=secret"
        );
    }

    #[test]
    fn rejects_a_truncated_zip_download() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut archive = zip::ZipWriter::new(path.reopen().unwrap());
        archive
            .start_file("chart.osu", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"contents").unwrap();
        archive.finish().unwrap();
        let mut bytes = std::fs::read(path.path()).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(path.path(), bytes).unwrap();

        assert!(verify_archive_complete(path.path()).is_err());
    }

    #[test]
    fn slow_download_threshold_triggers_reconnect() {
        assert!(super::is_below_minimum_download_rate(
            Duration::from_secs(60),
            59 * 1024 * 1024
        ));
        assert!(!super::is_below_minimum_download_rate(
            Duration::from_secs(60),
            60 * 1024 * 1024
        ));
    }

    #[test]
    fn identifies_only_standard_beatmaps_as_supported() {
        assert!(is_standard_beatmap(b"[General]\nMode: 0\n"));
        assert!(!is_standard_beatmap(b"[General]\nMode: 3\n"));
        assert!(!is_standard_beatmap(b"[General]\nMode: 1\n"));
        assert!(is_standard_beatmap(b"[General]\n"));
    }

    #[test]
    fn identifies_known_nonstandard_pack_ids() {
        assert!(is_known_nonstandard_pack_id("ST421"));
        assert!(is_known_nonstandard_pack_id("SM370"));
        assert!(is_known_nonstandard_pack_id("SC164"));
        assert!(!is_known_nonstandard_pack_id("S1845"));
        assert!(!is_known_nonstandard_pack_id("A26"));
    }

    #[test]
    fn concurrent_download_emits_a_finished_event_for_skipped_packs() {
        let root = tempfile::tempdir().unwrap();
        let store = FeatureStore::open(root.path()).unwrap();
        let cookie = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            cookie.path(),
            ".ppy.sh\tTRUE\t/\tTRUE\t0\tosu_session\tsecret\n",
        )
        .unwrap();
        let events = Mutex::new(Vec::new());

        let reports = PackImporter::new(None)
            .unwrap()
            .download_packs_concurrently_with_progress(
                &store,
                &["ST421".to_owned()],
                cookie.path(),
                1,
                |event| events.lock().unwrap().push(event),
            )
            .unwrap();

        assert_eq!(reports.len(), 1);
        assert!(reports[0].skipped);
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [
                PackDownloadEvent::Started { pack_id },
                PackDownloadEvent::Finished(report)
            ] if pack_id == "ST421" && report.skipped
        ));
        assert!(store.pack_is_excluded("ST421").unwrap());
    }

    #[test]
    fn sequential_import_skips_known_nonstandard_packs_without_a_request() {
        let root = tempfile::tempdir().unwrap();
        let mut store = FeatureStore::open(root.path()).unwrap();

        let report = PackImporter::new(None)
            .unwrap()
            .download_and_import_with_progress(
                &mut store,
                &Analyzer::new(AnalyzerConfig::default()),
                "ST421",
                root.path(),
                |_| {},
            )
            .unwrap();

        assert_eq!(report, super::PackImportReport::default());
        assert!(store.pack_is_excluded("ST421").unwrap());
    }
}
