use std::{
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use reqwest::{blocking::Client, header::COOKIE};
use zip::ZipArchive;

use crate::{Analyzer, FeatureStore};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackImportReport {
    pub processed: usize,
    pub inserted: usize,
    pub skipped: usize,
    pub failed: usize,
}

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
        let cookie = read_netscape_cookie(cookie_file)?;
        let url = format!("https://osu.ppy.sh/beatmaps/packs/{pack_id}/download");
        store.mark_pack(pack_id, &url, "downloading", None)?;
        let temporary = store.root().join("tmp").join(format!("{pack_id}.part"));
        let response = self
            .client
            .get(&url)
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?;
        let bytes = response.bytes()?;
        {
            let mut file = File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        match self.import_archive(store, analyzer, &temporary) {
            Ok(report) => {
                fs::remove_file(&temporary)?;
                store.mark_pack(pack_id, &url, "complete", None)?;
                Ok(report)
            }
            Err(error) => {
                store.mark_pack(
                    pack_id,
                    &url,
                    "failed",
                    Some("archive processing failed; temporary archive retained for retry"),
                )?;
                Err(error)
            }
        }
    }

    pub fn import_archive(
        &self,
        store: &mut FeatureStore,
        analyzer: &Analyzer,
        archive_path: &Path,
    ) -> Result<PackImportReport> {
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
        Ok(report)
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
fn read_netscape_cookie(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path).context("read cookie file")?;
    let values = content
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let parts = line.split('\t').collect::<Vec<_>>();
            (parts.len() >= 7 && parts[0].contains("osu.ppy.sh"))
                .then(|| format!("{}={}", parts[5], parts[6]))
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        bail!("cookie file contains no osu.ppy.sh session cookies");
    }
    Ok(values.join("; "))
}
