use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use arrow_array::{ArrayRef, Float32Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use clap::{Parser, Subcommand};
use osu_difficulty_lab::{
    Analyzer, AnalyzerConfig, FeatureStore, PackImporter, SimilarityQuery, SimilarityStore,
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
    },
    ValidateCookie {
        #[arg(long)]
        cookie_file: PathBuf,
    },
    IngestLocal {
        data_dir: PathBuf,
        archive: PathBuf,
    },
    IngestPacks {
        data_dir: PathBuf,
        #[arg(long)]
        cookie_file: PathBuf,
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
fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { data_dir } => {
            FeatureStore::open(data_dir)?;
        }
        Command::CatalogSync { output } => {
            let importer = PackImporter::new(None)?;
            let types = [
                "standard",
                "featured",
                "tournament",
                "loved",
                "chart",
                "theme",
                "artist",
            ];
            let ids = importer.sync_catalog(&types)?;
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
        Command::IngestPacks {
            data_dir,
            cookie_file,
            pack_ids,
        } => {
            let mut store = FeatureStore::open(data_dir)?;
            let importer = PackImporter::new(Some(&cookie_file))?;
            let analyzer = Analyzer::new(AnalyzerConfig::default());
            for id in pack_ids {
                let mut last_draw = Instant::now() - Duration::from_secs(1);
                let result = importer.download_and_import_with_progress(
                    &mut store,
                    &analyzer,
                    &id,
                    &cookie_file,
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
                        eprint!(
                            "\r{id} attempt {}/3: {:.1} / {total} · {:.2} MiB/s",
                            progress.attempt,
                            progress.downloaded_bytes as f64 / 1024.0 / 1024.0,
                            progress.bytes_per_second / 1024.0 / 1024.0,
                        );
                        let _ = io::stderr().flush();
                        last_draw = now;
                    },
                );
                eprintln!();
                let report = result?;
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
                "beatmap_id,beatmapset_id,aim,speed,reading,flashlight,overlap,bpm,ar,object_density\n",
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
                    d.flashlight,
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
        Field::new("flashlight", DataType::Float32, false),
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
