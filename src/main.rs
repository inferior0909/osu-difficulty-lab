use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use arrow_array::{ArrayRef, Float32Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use clap::{Parser, Subcommand, ValueEnum};
use osu_difficulty_lab::{
    Analyzer, AnalyzerConfig, BeatmapMetadata, DownloadProgress, FeatureStore, ManiaAnalyzeError,
    ManiaAnalyzer, ManiaBeatmapMetadata, ManiaFeatureStore, ManiaNormalizer, ManiaRawFeatureRecord,
    ManiaSimilarityQuery, ManiaSimilarityStore, PackDownloadEvent, PackDownloadReport,
    PackDownloadSource, PackImporter, RawFeatureRecord, SimilarityQuery, SimilarityStore,
    build_main_index, build_mania_index, export_mania_csv, export_mania_parquet,
    fit_mania_normalizer, fit_normalizer, validate_mania_index_coverage,
};
use sha2::{Digest, Sha256};

const REANALYZE_TIMEOUT: Duration = Duration::from_secs(30);
const MANIA_REANALYZE_TIMEOUT: Duration = Duration::from_secs(30);

struct ReanalysisWorker {
    requests: mpsc::Sender<Vec<u8>>,
    results: mpsc::Receiver<Result<(BeatmapMetadata, RawFeatureRecord)>>,
}

impl ReanalysisWorker {
    fn new() -> Self {
        let (requests, request_rx) = mpsc::channel::<Vec<u8>>();
        let (result_tx, results) = mpsc::channel();
        thread::spawn(move || {
            let analyzer = Analyzer::new(AnalyzerConfig::default());
            while let Ok(bytes) = request_rx.recv() {
                if result_tx.send(analyzer.analyze_bytes(&bytes)).is_err() {
                    break;
                }
            }
        });
        Self { requests, results }
    }

    fn analyze(&mut self, bytes: Vec<u8>) -> Result<(BeatmapMetadata, RawFeatureRecord)> {
        if self.requests.send(bytes).is_err() {
            *self = Self::new();
            return Err(anyhow!("analysis worker stopped unexpectedly"));
        }
        match self.results.recv_timeout(REANALYZE_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Dropping the channels detaches a stuck dependency call. The
                // replacement worker lets the remaining dataset continue.
                *self = Self::new();
                Err(anyhow!(
                    "analysis timed out after {} seconds",
                    REANALYZE_TIMEOUT.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                *self = Self::new();
                Err(anyhow!("analysis worker stopped unexpectedly"))
            }
        }
    }
}

struct ManiaTimedWorker {
    requests: mpsc::Sender<(Vec<u8>, Option<u64>)>,
    results: mpsc::Receiver<
        std::result::Result<(ManiaBeatmapMetadata, ManiaRawFeatureRecord), ManiaAnalyzeError>,
    >,
}

impl ManiaTimedWorker {
    fn new() -> Self {
        let (requests, request_rx) = mpsc::channel::<(Vec<u8>, Option<u64>)>();
        let (result_tx, results) = mpsc::channel();
        thread::spawn(move || {
            let analyzer = ManiaAnalyzer::new();
            while let Ok((bytes, beatmap_id)) = request_rx.recv() {
                let result = match beatmap_id {
                    Some(beatmap_id) => analyzer.analyze_bytes_with_beatmap_id(&bytes, beatmap_id),
                    None => analyzer.analyze_bytes(&bytes),
                };
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        });
        Self { requests, results }
    }

    fn analyze(
        &mut self,
        bytes: Vec<u8>,
        source_beatmap_id: Option<u64>,
    ) -> Result<std::result::Result<(ManiaBeatmapMetadata, ManiaRawFeatureRecord), ManiaAnalyzeError>>
    {
        if self.requests.send((bytes, source_beatmap_id)).is_err() {
            *self = Self::new();
            return Err(anyhow!("mania analysis worker stopped unexpectedly"));
        }
        match self.results.recv_timeout(MANIA_REANALYZE_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                *self = Self::new();
                Err(anyhow!(
                    "mania analysis timed out after {} seconds",
                    MANIA_REANALYZE_TIMEOUT.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                *self = Self::new();
                Err(anyhow!("mania analysis worker stopped unexpectedly"))
            }
        }
    }
}

enum ManiaReanalysisOutcome {
    Analyzed(Box<(ManiaBeatmapMetadata, ManiaRawFeatureRecord)>),
    Unsupported,
    Failed(String),
    Skipped,
}

#[derive(Parser)]
#[command(
    name = "osu-difficulty-lab",
    about = "Five-dimensional osu!standard research dataset and similarity index"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Init {
        data_dir: PathBuf,
    },
    CatalogSync {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        standard_only: bool,
        /// Route network requests through an HTTP(S) or SOCKS5 proxy.
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,
    },
    ValidateCookie {
        #[arg(long)]
        cookie_file: PathBuf,
    },
    IngestLocal {
        data_dir: PathBuf,
        archive: PathBuf,
    },
    /// Recompute the current analyzer version from retained beatmaps/*.osu files.
    Reanalyze {
        data_dir: PathBuf,
    },
    IngestPacks {
        data_dir: PathBuf,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long, value_enum, default_value_t = DownloadSourceArg::Official)]
        source: DownloadSourceArg,
        /// Route network requests through an HTTP(S) or SOCKS5 proxy.
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,
        #[arg(required = true)]
        pack_ids: Vec<String>,
    },
    DownloadPacks {
        data_dir: PathBuf,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Route network requests through an HTTP(S) or SOCKS5 proxy.
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,
        #[arg(required = true)]
        pack_ids: Vec<String>,
    },
    IngestDownloaded {
        data_dir: PathBuf,
        #[arg(required = true)]
        pack_ids: Vec<String>,
    },
    NormalizerFit {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    IndexBuild {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    Query {
        data_dir: PathBuf,
        beatmap_id: u64,
        #[arg(long, default_value_t = 1)]
        version: u32,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    ExportCsv {
        data_dir: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    ExportParquet {
        data_dir: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    Doctor {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    /// Initialize the independent osu!mania feature store.
    ManiaInit {
        data_dir: PathBuf,
    },
    /// Analyze retained 4K/6K/7K beatmaps/*.osu into mania raw features.
    ManiaReanalyze {
        data_dir: PathBuf,
        #[arg(long)]
        threads: Option<usize>,
    },
    ManiaNormalizerFit {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    ManiaIndexBuild {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    ManiaQuery {
        data_dir: PathBuf,
        #[arg(long, conflicts_with = "file")]
        beatmap_id: Option<u64>,
        #[arg(long, conflicts_with = "beatmap_id")]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        version: u32,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        include_same_set: bool,
    },
    ManiaExportCsv {
        data_dir: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    ManiaExportParquet {
        data_dir: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
    ManiaDoctor {
        data_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        version: u32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DownloadSourceArg {
    Official,
    Hinamizawa,
}

impl From<DownloadSourceArg> for PackDownloadSource {
    fn from(value: DownloadSourceArg) -> Self {
        match value {
            DownloadSourceArg::Official => Self::Official,
            DownloadSourceArg::Hinamizawa => Self::Hinamizawa,
        }
    }
}

struct DownloadBatchDisplay {
    active: HashSet<String>,
    progress: HashMap<String, DownloadProgress>,
    completed: usize,
    completed_bytes: u64,
    total: usize,
}

impl DownloadBatchDisplay {
    fn new(total: usize) -> Self {
        Self {
            active: HashSet::new(),
            progress: HashMap::new(),
            completed: 0,
            completed_bytes: 0,
            total,
        }
    }

    fn handle(&mut self, event: PackDownloadEvent) {
        match event {
            PackDownloadEvent::Started { pack_id } => {
                self.active.insert(pack_id);
            }
            PackDownloadEvent::Progress { pack_id, progress } => {
                self.active.insert(pack_id.clone());
                self.progress.insert(pack_id, progress);
            }
            PackDownloadEvent::Finished(report) => {
                self.active.remove(&report.pack_id);
                if let Some(progress) = self.progress.remove(&report.pack_id) {
                    self.completed_bytes += progress.downloaded_bytes;
                }
                self.completed += 1;
                print_pack_download_report(&report);
            }
        }
    }

    fn draw(&self) {
        let staged_bytes = self.completed_bytes
            + self
                .progress
                .values()
                .map(|progress| progress.downloaded_bytes)
                .sum::<u64>();
        let bytes_per_second = self
            .progress
            .values()
            .map(|progress| progress.bytes_per_second)
            .sum::<f64>();
        println!(
            "download progress: {}/{} finished, {} active, {:.1} MiB staged, {:.2} MiB/s",
            self.completed,
            self.total,
            self.active.len(),
            staged_bytes as f64 / 1024.0 / 1024.0,
            bytes_per_second / 1024.0 / 1024.0,
        );
        let _ = io::stdout().flush();
    }
}

fn print_pack_download_report(report: &PackDownloadReport) {
    if report.succeeded {
        println!("{}: downloaded", report.pack_id);
    } else if report.already_complete {
        println!("{}: already complete", report.pack_id);
    } else if report.skipped {
        println!("{}: skipped (no osu!standard beatmaps)", report.pack_id);
    } else {
        eprintln!(
            "{}: failed: {}",
            report.pack_id,
            report.error.as_deref().unwrap_or("unknown error")
        );
    }
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { data_dir } => {
            FeatureStore::open(data_dir)?;
        }
        Command::CatalogSync {
            output,
            standard_only,
            proxy,
        } => {
            let importer = PackImporter::with_proxy(None, proxy.as_deref())?;
            let types = [
                "standard",
                "featured",
                "tournament",
                "loved",
                "chart",
                "theme",
                "artist",
            ];
            let ids = if standard_only {
                importer.sync_standard_catalog(&types)?
            } else {
                importer.sync_catalog(&types)?
            };
            fs::write(output, ids.join("\n"))?;
        }
        Command::ValidateCookie { cookie_file } => {
            PackImporter::new(Some(&cookie_file))?.validate_cookie_file(&cookie_file)?;
            println!("Netscape osu.ppy.sh Cookie format is valid.");
        }
        Command::IngestLocal { data_dir, archive } => {
            let mut store = FeatureStore::open(data_dir)?;
            let report = PackImporter::new(None)?.import_archive(
                &mut store,
                &Analyzer::new(AnalyzerConfig::default()),
                &archive,
            )?;
            println!(
                "processed={} inserted={} skipped={} failed={}",
                report.processed, report.inserted, report.skipped, report.failed
            );
        }
        Command::Reanalyze { data_dir } => {
            let mut store = FeatureStore::open(&data_dir)?;
            if store.prepare_reanalysis()? {
                println!(
                    "algorithm snapshot changed; invalidated Analyzer v{} records and indexes",
                    osu_difficulty_lab::ANALYZER_VERSION
                );
            }
            let mut worker = ReanalysisWorker::new();
            let mut paths = fs::read_dir(data_dir.join("beatmaps"))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|extension| extension == "osu"))
                .collect::<Vec<_>>();
            paths.sort();
            let total = paths.len();
            let mut inserted = 0_usize;
            let mut skipped = 0_usize;
            let mut failed = Vec::new();
            for (index, path) in paths.into_iter().enumerate() {
                let result = (|| -> Result<bool> {
                    let bytes = fs::read(&path)?;
                    if let Some(beatmap_id) = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        let checksum = hex::encode(Sha256::digest(&bytes));
                        if store.current_analysis_matches(beatmap_id, &checksum)? {
                            return Ok(false);
                        }
                    }
                    let (metadata, record) = worker.analyze(bytes)?;
                    store.append_raw(&metadata, &record)
                })();
                match result {
                    Ok(true) => inserted += 1,
                    Ok(false) => skipped += 1,
                    Err(error) => failed.push(format!("{}: {error}", path.display())),
                }
                if (index + 1) % 1000 == 0 || index + 1 == total {
                    println!(
                        "reanalyze progress: {}/{} inserted={} skipped={} failed={}",
                        index + 1,
                        total,
                        inserted,
                        skipped,
                        failed.len()
                    );
                }
            }
            if !failed.is_empty() {
                fs::write(data_dir.join("reanalyze-failures.txt"), failed.join("\n"))?;
                anyhow::bail!(
                    "{} of {} retained beatmaps failed; see reanalyze-failures.txt",
                    failed.len(),
                    total
                );
            }
            let failure_log = data_dir.join("reanalyze-failures.txt");
            if failure_log.exists() {
                fs::remove_file(failure_log)?;
            }
        }
        Command::IngestPacks {
            data_dir,
            cookie_file,
            source,
            proxy,
            pack_ids,
        } => {
            let mut store = FeatureStore::open(data_dir)?;
            let importer = PackImporter::with_proxy(Some(&cookie_file), proxy.as_deref())?;
            let analyzer = Analyzer::new(AnalyzerConfig::default());
            for id in pack_ids {
                let mut last_draw = Instant::now() - Duration::from_secs(1);
                let mut drew_progress = false;
                let result = importer.download_and_import_from_source_with_progress(
                    &mut store,
                    &analyzer,
                    &id,
                    &cookie_file,
                    source.into(),
                    |progress| {
                        let now = Instant::now();
                        if now.duration_since(last_draw) < Duration::from_millis(250)
                            && progress
                                .total_bytes
                                .is_none_or(|total| progress.downloaded_bytes < total)
                        {
                            return;
                        }
                        let total = progress
                            .total_bytes
                            .map(|value| format!("{:.1} MiB", value as f64 / 1024.0 / 1024.0))
                            .unwrap_or_else(|| "? MiB".into());
                        let (bar, percentage) = progress.total_bytes.map_or_else(
                            || ("[????????????????????????????]".to_owned(), "  ?.?%".to_owned()),
                            |total| {
                                let ratio = if total == 0 {
                                    0.0
                                } else {
                                    progress.downloaded_bytes as f64 / total as f64
                                };
                                let filled = (ratio * 28.0).round().clamp(0.0, 28.0) as usize;
                                (
                                    format!("[{}{}]", "#".repeat(filled), "-".repeat(28 - filled)),
                                    format!("{:5.1}%", ratio * 100.0),
                                )
                            },
                        );
                        print!(
                            "\rdownload progress: {id} attempt {}/3 {bar} {percentage} {:.1} / {total} - {:.2} MiB/s",
                            progress.attempt,
                            progress.downloaded_bytes as f64 / 1024.0 / 1024.0,
                            progress.bytes_per_second / 1024.0 / 1024.0,
                        );
                        let _ = io::stdout().flush();
                        last_draw = now;
                        drew_progress = true;
                    },
                );
                if drew_progress {
                    println!();
                }
                let report = result?;
                println!(
                    "{id}: processed={} inserted={} skipped={} failed={}",
                    report.processed, report.inserted, report.skipped, report.failed
                );
            }
        }
        Command::DownloadPacks {
            data_dir,
            cookie_file,
            concurrency,
            proxy,
            pack_ids,
        } => {
            let store = FeatureStore::open(data_dir)?;
            let display = Mutex::new(DownloadBatchDisplay::new(pack_ids.len()));
            let importer = PackImporter::with_proxy(Some(&cookie_file), proxy.as_deref())?;
            thread::scope(|scope| {
                let (stop_tx, stop_rx) = mpsc::channel::<()>();
                let monitor_display = &display;
                scope.spawn(move || {
                    loop {
                        match stop_rx.recv_timeout(Duration::from_secs(1)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => monitor_display
                                .lock()
                                .expect("download progress display poisoned")
                                .draw(),
                        }
                    }
                });
                let result = importer.download_packs_concurrently_with_progress(
                    &store,
                    &pack_ids,
                    &cookie_file,
                    concurrency,
                    |event| {
                        display
                            .lock()
                            .expect("download progress display poisoned")
                            .handle(event);
                    },
                );
                let _ = stop_tx.send(());
                result
            })?;
        }
        Command::IngestDownloaded { data_dir, pack_ids } => {
            let mut store = FeatureStore::open(data_dir)?;
            let importer = PackImporter::new(None)?;
            let analyzer = Analyzer::new(AnalyzerConfig::default());
            for id in pack_ids {
                let report = importer.import_downloaded_pack(&mut store, &analyzer, &id)?;
                println!(
                    "{id}: processed={} inserted={} skipped={} failed={}",
                    report.processed, report.inserted, report.skipped, report.failed
                );
            }
        }
        Command::NormalizerFit { data_dir, version } => {
            let mut store = FeatureStore::open(data_dir)?;
            let normalizer = fit_normalizer(&mut store, version)?;
            println!("wrote normalization v{}", normalizer.version);
        }
        Command::IndexBuild { data_dir, version } => {
            let store = FeatureStore::open(data_dir)?;
            build_main_index(&store, version)?;
        }
        Command::Query {
            data_dir,
            beatmap_id,
            version,
            limit,
        } => {
            let store = FeatureStore::open(data_dir)?;
            let query = SimilarityQuery {
                beatmap_id,
                result_limit: limit,
                ..SimilarityQuery::default()
            };
            for result in SimilarityStore::open(store.root(), version)?.query(&store, query)? {
                println!(
                    "{}\tset={}\tdistance={:.5}\td1={:.5}\td2={:.5}",
                    result.beatmap_id,
                    result.beatmapset_id,
                    result.final_distance,
                    result.difficulty_distance,
                    result.base_distance
                );
            }
        }
        Command::ExportCsv {
            data_dir,
            output,
            version,
        } => {
            let store = FeatureStore::open(data_dir)?;
            let mut text = String::from(
                "beatmap_id,beatmapset_id,aim,speed,reading,slider,overlap,bpm,ar,object_density\n",
            );
            for record in store.normalized_records(version)? {
                let d = record.difficulty;
                let b = record.base;
                text.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{}\n",
                    record.beatmap_id,
                    record.beatmapset_id,
                    d.aim,
                    d.speed,
                    d.reading,
                    d.slider,
                    d.overlap,
                    b.bpm,
                    b.ar,
                    b.object_density
                ));
            }
            fs::write(output, text)?;
        }
        Command::ExportParquet {
            data_dir,
            output,
            version,
        } => {
            let store = FeatureStore::open(data_dir)?;
            export_parquet(&output, &store.normalized_records(version)?)?;
        }
        Command::Doctor { data_dir, version } => {
            let store = FeatureStore::open(data_dir)?;
            let count = store.normalized_records(version)?.len();
            let _ = SimilarityStore::open(store.root(), version)?;
            store.validate_star_section_stats(version)?;
            osu_difficulty_lab::validate_index_coverage(&store, version)?;
            println!("healthy: {count} normalized records");
        }
        Command::ManiaInit { data_dir } => {
            ManiaFeatureStore::open(data_dir)?;
        }
        Command::ManiaReanalyze { data_dir, threads } => {
            run_mania_reanalysis(&data_dir, threads)?;
        }
        Command::ManiaNormalizerFit { data_dir, version } => {
            let mut store = ManiaFeatureStore::open(data_dir)?;
            let normalizer = fit_mania_normalizer(&mut store, version)?;
            println!("wrote mania normalization v{}", normalizer.version);
        }
        Command::ManiaIndexBuild { data_dir, version } => {
            let store = ManiaFeatureStore::open(data_dir)?;
            build_mania_index(&store, version)?;
            println!("wrote mania bucket index v{version}");
        }
        Command::ManiaQuery {
            data_dir,
            beatmap_id,
            file,
            version,
            limit,
            include_same_set,
        } => {
            if beatmap_id.is_none() == file.is_none() {
                anyhow::bail!("provide exactly one of --beatmap-id or --file");
            }
            let store = ManiaFeatureStore::open(&data_dir)?;
            let similarity = ManiaSimilarityStore::open(&data_dir, version)?;
            let query = ManiaSimilarityQuery {
                result_limit: limit,
                include_same_set,
            };
            let results = if let Some(beatmap_id) = beatmap_id {
                similarity.query_by_id(&store, beatmap_id, query)?
            } else {
                let path = file.expect("checked above");
                let bytes = fs::read(&path)?;
                let source_beatmap_id = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u64>().ok());
                let analyzer = ManiaAnalyzer::new();
                let (_, raw) = match source_beatmap_id {
                    Some(beatmap_id) => analyzer.analyze_bytes_with_beatmap_id(&bytes, beatmap_id),
                    None => analyzer.analyze_bytes(&bytes),
                }
                .map_err(anyhow::Error::new)?;
                let target = ManiaNormalizer::load(&data_dir, version)?.transform(&raw)?;
                similarity.query_record(&store, target, query)?
            };
            for result in results {
                println!(
                    "{}\tset={}\t{}K\tband={}\tpct={:.4}\tfamily={}\tpattern={}\tdistance={:.5}\tskill={:.5}\tpatterns={:.5}\tstructure={:.5}\tdifficulty={:.5}\tcontext={:.5}\t{} - {} [{}]",
                    result.beatmap_id,
                    result.beatmapset_id,
                    result.key_count,
                    result.difficulty_band,
                    result.difficulty_percentile,
                    result.mode_family.as_str(),
                    result.dominant_pattern.as_str(),
                    result.final_distance,
                    result.components.skill,
                    result.components.pattern,
                    result.components.structure,
                    result.components.difficulty,
                    result.components.context,
                    result.artist,
                    result.title,
                    result.version,
                );
            }
        }
        Command::ManiaExportCsv {
            data_dir,
            output,
            version,
        } => {
            let store = ManiaFeatureStore::open(data_dir)?;
            export_mania_csv(output, &store.normalized_records(version)?)?;
        }
        Command::ManiaExportParquet {
            data_dir,
            output,
            version,
        } => {
            let store = ManiaFeatureStore::open(data_dir)?;
            export_mania_parquet(output, &store.normalized_records(version)?)?;
        }
        Command::ManiaDoctor { data_dir, version } => {
            let store = ManiaFeatureStore::open(&data_dir)?;
            let records = store.normalized_records(version)?;
            let count = records.len();
            let normalizer = ManiaNormalizer::load(&data_dir, version)?;
            if normalizer.version != version {
                anyhow::bail!("mania normalizer version mismatch");
            }
            validate_mania_index_coverage(&store, version)?;
            let _ = ManiaSimilarityStore::open(&data_dir, version)?;
            let (eligible, unsupported, failed) = store.scan_counts()?;
            if eligible != count {
                anyhow::bail!(
                    "mania scan has {eligible} eligible maps but only {count} normalized records"
                );
            }
            if failed != 0 {
                anyhow::bail!("mania scan still has {failed} failed beatmaps");
            }
            let mut key_counts = [0_usize; 3];
            let mut band_counts = [0_usize; 10];
            let mut family_counts = [0_usize; 4];
            for record in records {
                let key_index = match record.key_count {
                    4 => 0,
                    6 => 1,
                    7 => 2,
                    key_count => anyhow::bail!("indexed unsupported key count {key_count}K"),
                };
                key_counts[key_index] += 1;
                band_counts[record.difficulty_band as usize] += 1;
                family_counts[record.mode_family as usize] += 1;
            }
            println!(
                "healthy: normalized={count} eligible={eligible} unsupported={unsupported} failed={failed} keys=4K:{},6K:{},7K:{} families=RC:{},HB:{},Mix:{},LN:{} bands={band_counts:?}",
                key_counts[0],
                key_counts[1],
                key_counts[2],
                family_counts[0],
                family_counts[1],
                family_counts[2],
                family_counts[3],
            );
        }
    };
    Ok(())
}

fn run_mania_reanalysis(data_dir: &PathBuf, requested_threads: Option<usize>) -> Result<()> {
    let mut store = ManiaFeatureStore::open(data_dir)?;
    if store.prepare_reanalysis()? {
        println!(
            "mania algorithm snapshot changed; invalidated Analyzer v{} indexes",
            osu_difficulty_lab::MANIA_ANALYZER_VERSION
        );
    }
    let mut paths = fs::read_dir(data_dir.join("beatmaps"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "osu"))
        .collect::<Vec<_>>();
    paths.sort();
    let total = paths.len();
    if total == 0 {
        anyhow::bail!(
            "no .osu files found in {}",
            data_dir.join("beatmaps").display()
        );
    }
    let worker_count = requested_threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, usize::from))
        .clamp(1, 32);
    println!("mania reanalysis: {total} files, {worker_count} workers");

    let (task_tx, task_rx) = mpsc::channel::<(usize, PathBuf)>();
    let task_rx = Arc::new(Mutex::new(task_rx));
    let (result_tx, result_rx) = mpsc::channel::<(usize, PathBuf, ManiaReanalysisOutcome)>();
    let mut outcomes = (0..total).map(|_| None).collect::<Vec<_>>();
    let mut scheduled = 0_usize;

    for (index, path) in paths.into_iter().enumerate() {
        let retained_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u64>().ok());
        if let Some(beatmap_id) = retained_id
            && store.has_current_analysis(beatmap_id)?
        {
            let bytes = fs::read(&path)?;
            let checksum = hex::encode(Sha256::digest(&bytes));
            if store.current_analysis_matches(beatmap_id, &checksum)? {
                outcomes[index] = Some(ManiaReanalysisOutcome::Skipped);
                continue;
            }
        }
        task_tx.send((index, path))?;
        scheduled += 1;
    }
    drop(task_tx);

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let task_rx = Arc::clone(&task_rx);
        let result_tx = result_tx.clone();
        handles.push(thread::spawn(move || {
            let mut worker = ManiaTimedWorker::new();
            loop {
                let task = {
                    let receiver = task_rx.lock().expect("mania task receiver poisoned");
                    receiver.recv()
                };
                let Ok((index, path)) = task else {
                    break;
                };
                let source_beatmap_id = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u64>().ok());
                let outcome = match fs::read(&path) {
                    Ok(bytes) => match worker.analyze(bytes, source_beatmap_id) {
                        Ok(Ok((metadata, record))) => {
                            ManiaReanalysisOutcome::Analyzed(Box::new((metadata, record)))
                        }
                        Ok(Err(ManiaAnalyzeError::UnsupportedKeyCount(_))) => {
                            ManiaReanalysisOutcome::Unsupported
                        }
                        Ok(Err(error)) => ManiaReanalysisOutcome::Failed(error.to_string()),
                        Err(error) => ManiaReanalysisOutcome::Failed(error.to_string()),
                    },
                    Err(error) => ManiaReanalysisOutcome::Failed(error.to_string()),
                };
                if result_tx.send((index, path, outcome)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(result_tx);

    let mut received = 0_usize;
    while received < scheduled {
        let (index, path, outcome) = result_rx
            .recv()
            .map_err(|_| anyhow!("mania worker pool stopped before all files completed"))?;
        outcomes[index] = Some(match outcome {
            ManiaReanalysisOutcome::Failed(error) => {
                ManiaReanalysisOutcome::Failed(format!("{}: {error}", path.display()))
            }
            other => other,
        });
        received += 1;
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow!("mania analysis worker panicked"))?;
    }

    let mut inserted = 0_usize;
    let mut skipped = 0_usize;
    let mut unsupported = 0_usize;
    let mut failures = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        match outcome.ok_or_else(|| anyhow!("mania file {} has no analysis outcome", index + 1))? {
            ManiaReanalysisOutcome::Analyzed(analyzed) => {
                let (metadata, record) = *analyzed;
                if store.append_raw(&metadata, &record)? {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
            ManiaReanalysisOutcome::Unsupported => unsupported += 1,
            ManiaReanalysisOutcome::Failed(error) => failures.push(error),
            ManiaReanalysisOutcome::Skipped => skipped += 1,
        }
        if index + 1 == total || (index + 1) % 1000 == 0 {
            println!(
                "mania reanalyze progress: {}/{} inserted={} skipped={} unsupported={} failed={}",
                index + 1,
                total,
                inserted,
                skipped,
                unsupported,
                failures.len(),
            );
        }
    }
    let eligible = inserted + skipped;
    store.set_scan_counts(eligible, unsupported, failures.len())?;
    let failure_log = data_dir.join("mania-reanalyze-failures.txt");
    if failures.is_empty() {
        if failure_log.exists() {
            fs::remove_file(failure_log)?;
        }
    } else {
        fs::write(&failure_log, failures.join("\n"))?;
        anyhow::bail!(
            "{} of {} mania files failed; see {}",
            failures.len(),
            total,
            failure_log.display()
        );
    }
    println!("mania reanalysis complete: eligible={eligible} unsupported={unsupported} failed=0");
    Ok(())
}

fn export_parquet(
    output: &PathBuf,
    records: &[osu_difficulty_lab::BeatmapFeatureRecord],
) -> Result<()> {
    use parquet::arrow::ArrowWriter;
    let schema = Arc::new(Schema::new(vec![
        Field::new("beatmap_id", DataType::Int64, false),
        Field::new("beatmapset_id", DataType::Int64, false),
        Field::new("aim", DataType::Float32, false),
        Field::new("speed", DataType::Float32, false),
        Field::new("reading", DataType::Float32, false),
        Field::new("slider", DataType::Float32, false),
        Field::new("overlap", DataType::Float32, false),
        Field::new("bpm", DataType::Float32, false),
        Field::new("ar", DataType::Float32, false),
        Field::new("object_density", DataType::Float32, false),
    ]));
    let ids = Int64Array::from_iter_values(records.iter().map(|record| record.beatmap_id as i64));
    let set_ids =
        Int64Array::from_iter_values(records.iter().map(|record| record.beatmapset_id as i64));
    let values = |index: usize| {
        Float32Array::from_iter_values(
            records
                .iter()
                .map(move |record| record.difficulty.as_array()[index]),
        )
    };
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(ids),
        Arc::new(set_ids),
        Arc::new(values(0)),
        Arc::new(values(1)),
        Arc::new(values(2)),
        Arc::new(values(3)),
        Arc::new(values(4)),
        Arc::new(Float32Array::from_iter_values(
            records.iter().map(|record| record.base.bpm),
        )),
        Arc::new(Float32Array::from_iter_values(
            records.iter().map(|record| record.base.ar),
        )),
        Arc::new(Float32Array::from_iter_values(
            records.iter().map(|record| record.base.object_density),
        )),
    ];
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    let mut writer = ArrowWriter::try_new(fs::File::create(output)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
