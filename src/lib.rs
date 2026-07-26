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
pub use importer::{PackImportReport, PackImporter};
pub use index::{SimilarityStore, build_main_index};
pub use normalizer::{Normalizer, fit_normalizer};
pub use storage::FeatureStore;
pub use types::*;

pub const ANALYZER_VERSION: u32 = 1;
pub const RAW_FEATURE_FILE: &str = "raw-features.bin";
