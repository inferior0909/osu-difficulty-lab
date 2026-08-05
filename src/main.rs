use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use arrow_array::{ArrayRef, Float32Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use clap::{Parser, Subcommand, ValueEnum};
use osu_difficulty_lab::{
    Analyzer, AnalyzerConfig, DownloadProgress, FeatureStore, PackDownloadEvent,
    PackDownloadReport, PackDownloadSource, PackImporter, SimilarityQuery, SimilarityStore,
    build_main_index, fit_normalizer,
};

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
            let analyzer = Analyzer::new(AnalyzerConfig::default());
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
                let result = fs::read(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| analyzer.analyze_bytes(&bytes))
                    .and_then(|(metadata, record)| store.append_raw(&metadata, &record));
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
            println!("healthy: {count} normalized records");
        }
    };
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
