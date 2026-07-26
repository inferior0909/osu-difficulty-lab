use std::{
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::{
    StatusCode,
    blocking::Client,
    header::{COOKIE, RANGE},
};
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

const MAX_DOWNLOAD_ATTEMPTS: u8 = 3;
const MIN_DOWNLOAD_BYTES_PER_SECOND: f64 = 1024.0 * 1024.0;
const SLOW_DOWNLOAD_WINDOW: Duration = Duration::from_secs(60);

pub struct PackImporter {
    client: Client,
}
impl PackImporter {
    pub fn new(cookie_file: Option<&Path>) -> Result<Self> {
        let client = Client::builder()
            .user_agent("osu-difficulty-lab/0.1 (research importer)")
            .timeout(std::time::Duration::from_secs(180))
            .build()?;
        let _ = cookie_file; // Cookies are attached per request; never stored by this crate.
        Ok(Self { client })
    }

    pub fn validate_cookie_file(&self, cookie_file: &Path) -> Result<()> {
        let _ = &self.client;
        read_netscape_cookie(cookie_file).map(|_| ())
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
        mut on_progress: F,
    ) -> Result<PackImportReport>
    where
        F: FnMut(DownloadProgress),
    {
        if store.pack_is_complete(pack_id)? {
            return Ok(PackImportReport::default());
        }
        let temporary = store.root().join("tmp").join(format!("{pack_id}.part"));
        if temporary.is_file() {
            store.mark_pack(pack_id, &temporary.to_string_lossy(), "processing", None)?;
            if let Ok(report) = self.import_archive(store, analyzer, &temporary) {
                fs::remove_file(&temporary)?;
                store.mark_pack(pack_id, "local-retry", "complete", None)?;
                return Ok(report);
            }
        }
        let cookie = read_netscape_cookie(cookie_file)?;
        let pack_page = format!("https://osu.ppy.sh/beatmaps/packs/{pack_id}");
        let mut final_error = String::new();
        for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
            let result = (|| -> Result<PackImportReport> {
                let url = self.resolve_pack_download_url(pack_id, &cookie)?;
                store.mark_pack(pack_id, &url, "downloading", None)?;
                self.download_to_file(&url, &cookie, &temporary, attempt, &mut on_progress)?;
                let report = self.import_archive(store, analyzer, &temporary)?;
                fs::remove_file(&temporary)?;
                store.mark_pack(pack_id, &url, "complete", None)?;
                Ok(report)
            })();
            match result {
                Ok(report) => return Ok(report),
                Err(error) => {
                    final_error = error.to_string();
                    let status = if attempt == MAX_DOWNLOAD_ATTEMPTS {
                        "failed"
                    } else {
                        "retrying"
                    };
                    store.mark_pack(pack_id, &pack_page, status, Some(&final_error))?;
                    if attempt < MAX_DOWNLOAD_ATTEMPTS {
                        std::thread::sleep(Duration::from_secs(2_u64.pow(attempt as u32)));
                    }
                }
            }
        }
        bail!(
            "official pack {pack_id} failed after {MAX_DOWNLOAD_ATTEMPTS} attempts: {final_error}"
        )
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
        let mut response = request.send()?.error_for_status()?;
        let is_resumed = existing_bytes > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
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
                    bail!(
                        "download rate {:.2} MiB/s stayed below 1.00 MiB/s for 60 seconds; reconnecting",
                        rate / 1024.0 / 1024.0
                    );
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
        file.sync_all()?;
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
        decompress_file_with_extract_fn(archive_path, scratch, |entry, reader, _destination| {
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
        })
        .with_context(|| format!("extract 7z archive {}", archive_path.display()))?;
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
        match analyzer
            .analyze_bytes(bytes)
            .and_then(|(metadata, record)| {
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

fn extract_download_url(body: &str) -> Option<String> {
    body.split("href=\"")
        .skip(1)
        .filter_map(|fragment| fragment.split_once('"').map(|(url, _)| url))
        .map(|url| url.replace("&amp;", "&"))
        .find(|url| {
            url.starts_with("https://packs.ppy.sh/") || url.starts_with("https://dl.osu.ppy.sh/")
        })
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

#[cfg(test)]
mod tests {
    use super::extract_download_url;
    use std::time::Duration;

    #[test]
    fn extracts_authenticated_pack_download() {
        let html = r#"<a href="https://packs.ppy.sh/S1%20-%20test.zip?sig=one&amp;expires=two">download</a>"#;
        assert_eq!(
            extract_download_url(html).as_deref(),
            Some("https://packs.ppy.sh/S1%20-%20test.zip?sig=one&expires=two")
        );
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
}
