//! A versioned, local-only experimental data store for osu!standard difficulty research.
//! Imported source beatmap files are retained alongside feature records, metadata, and
//! indexes so research runs can be reproduced without downloading packs again.

mod analyzer;
mod importer;
mod index;
mod normalizer;
mod storage;
mod types;

pub use analyzer::{Analyzer, ParsedBeatmap};
pub use importer::{
    DownloadProgress, PackDownloadEvent, PackDownloadReport, PackDownloadSource, PackImportReport,
    PackImporter,
};
pub use index::{SimilarityStore, build_main_index, validate_index_coverage};
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
