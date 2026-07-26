//! A versioned, local-only experimental data store for osu!standard difficulty research.
//! The crate intentionally keeps source beatmap files transient: only feature records,
//! metadata, and indexes remain after a successful import.

mod analyzer;
mod importer;
mod index;
mod normalizer;
mod storage;
mod types;

pub use analyzer::{Analyzer, ParsedBeatmap};
pub use importer::{DownloadProgress, PackImportReport, PackImporter};
pub use index::{SimilarityStore, build_main_index};
pub use normalizer::{Normalizer, fit_normalizer};
pub use storage::FeatureStore;
pub use types::*;

/// Bump whenever a raw formula, dependency snapshot, or default weight changes.
pub const ANALYZER_VERSION: u32 = 2;
pub const ANALYZER_ALGORITHM_ID: &str = "five-dimension-baseline-v2";
pub const ROSU_PP_VERSION: &str = "4.0.1";
pub const READING_ALGORITHM_VERSION: &str = "reading-density-ar-section-v1";
pub const OVERLAP_ALGORITHM_VERSION: &str = "overlap-visibility-spatial-strain-v1";
pub const RAW_FEATURE_FILE: &str = "raw-features.bin";
