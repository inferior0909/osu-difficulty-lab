//! A versioned, local-only experimental data store for osu!standard difficulty research.
//! Imported source beatmap files are retained alongside feature records, metadata, and
//! indexes so research runs can be reproduced without downloading packs again.

mod analyzer;
mod importer;
mod index;
mod mania_analyzer;
mod mania_export;
mod mania_index;
mod mania_mod_export;
mod mania_normalizer;
mod mania_storage;
mod mania_types;
mod normalizer;
mod storage;
mod types;

pub use analyzer::{Analyzer, ParsedBeatmap};
pub use importer::{
    DownloadProgress, PackDownloadEvent, PackDownloadReport, PackDownloadSource, PackImportReport,
    PackImporter,
};
pub use index::{SimilarityStore, build_main_index, validate_index_coverage};
pub use mania_analyzer::{ManiaAnalyzeError, ManiaAnalyzer};
pub use mania_export::{export_mania_csv, export_mania_parquet};
pub use mania_index::{ManiaSimilarityStore, build_mania_index, validate_mania_index_coverage};
pub use mania_mod_export::{ManiaModFeatureRecord, export_mania_mod_features};
pub use mania_normalizer::{ManiaNormalizer, fit_mania_normalizer, overall_intensity};
pub use mania_pattern::{
    MANIA_MMA_ALGORITHM_ID, MANIA_MMA_ALGORITHM_VERSION, MANIA_MMA_SNAPSHOT, ManiaGameMod,
    ManiaMmaAnalysis, ManiaMmaBar, ManiaMmaCluster, ManiaMmaRecord, MmaFeatures, MmaReport,
    PatternCluster, analyze as analyze_mania_mma, analyze_record as analyze_mania_mma_record,
};
pub use mania_storage::ManiaFeatureStore;
pub use mania_types::*;
pub use normalizer::{Normalizer, fit_normalizer};
pub use storage::{FeatureStore, star_section};
pub use types::*;

/// Bump whenever a raw formula, dependency snapshot, or default weight changes.
pub const ANALYZER_VERSION: u32 = 4;
pub const ANALYZER_ALGORITHM_ID: &str = "five-dimension-slider-rosu-reading-v4";
/// Must exactly match OPP's runtime compatibility snapshot.
pub const ROSU_PP_VERSION: &str = "Apeuriox/rosu-pp@pp-rework-202607#9a073d29";
pub const READING_ALGORITHM_VERSION: &str = "rosu-reading-pp-rework-202607-v1";
pub const OVERLAP_ALGORITHM_VERSION: &str = "overlap-visibility-spatial-strain-v1";
pub const RAW_FEATURE_FILE: &str = "raw-features.bin";

/// Independent algorithm version for the osu!mania similarity dataset.
pub const MANIA_ANALYZER_VERSION: u32 = 1;
pub const MANIA_ANALYZER_ALGORITHM_ID: &str = "mania-roxy-interlude-similarity-v1";
pub const MANIA_RAW_FEATURE_FILE: &str = "mania-raw-features.bin";
/// Versioned key-pattern records from the osumania_map_analyser port, one per beatmap and clock rate.
pub const MANIA_MMA_FEATURE_FILE: &str = "mania-mma-features.bin";
