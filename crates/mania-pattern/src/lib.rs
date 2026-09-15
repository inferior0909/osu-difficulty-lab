//! Mania key-pattern analysis, ported from osumania_map_analyser.
//!
//! Module layout mirrors the original files:
//!
//! | mania_map_analyser | here |
//! | --- | --- |
//! | `js/parser/patternOsuParser.js` | [`parser`] |
//! | `js/patterns/primitives.js` | [`primitives`] |
//! | `js/patterns/patternsDef.js` | [`patterns`] |
//! | `js/patterns/clustering.js` | [`clustering`] |
//! | `js/patterns/categorise.js` | [`categorise`] |
//! | `js/patterns/summary.js` | [`summary`] |
//! | `js/patterns/config.js` | [`config`] |

pub mod categorise;
pub mod chart;
pub mod clustering;
pub mod config;
pub mod features;
pub mod parser;
pub mod patterns;
pub mod primitives;
pub mod rate;
pub mod similarity;
pub mod summary;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub use clustering::{PatternCluster, specific_clusters};
pub use features::MmaFeatures;
pub use rate::ManiaGameMod;
pub use summary::MmaReport;

/// Pinned mania_map_analyser revision, recorded as the data source.
pub const MANIA_MMA_SNAPSHOT: &str =
    "osumania_map_analyser@70e2bd92524e093ee94ca9cb6cc159ec223faa04";

/// Algorithm identifier; bump whenever rules, parameters or derived features change.
pub const MANIA_MMA_ALGORITHM_ID: &str = "mania-mma-pattern-v2";

#[derive(Debug, Clone)]
pub struct ManiaMmaAnalysis {
    pub key_count: u8,
    pub report: MmaReport,
    pub features: MmaFeatures,
}

/// `game_mod` selects the clock rate: DT/HT rescale the chart before analysis instead of scaling NoMod features.
pub fn analyze(text: &str, game_mod: ManiaGameMod) -> Result<ManiaMmaAnalysis> {
    let meta = lightweight_metadata(text);
    if meta.mode != Some(3) {
        bail!("only osu!mania mode is supported");
    }
    let Some(key_count) = meta.key_count else {
        bail!("missing CircleSize");
    };
    if !matches!(key_count, 4 | 6 | 7) {
        bail!("only 4K, 6K, and 7K are supported (found {key_count}K)");
    }

    let transformed = rate::transform_rate(text, game_mod.rate())?;
    let chart = parser::parse_osu_mania(&transformed)?;
    if chart.keys != key_count as usize {
        bail!(
            "key count changed while transforming: {} -> {}",
            key_count,
            chart.keys
        );
    }

    let report = summary::from_chart(&chart);
    let features = features::derive(&chart, &report);

    Ok(ManiaMmaAnalysis {
        key_count,
        report,
        features,
    })
}

/// Minimal metadata (mode and key count) so validation does not rerun the full analyser.
#[derive(Debug, Default)]
struct LightMetadata {
    mode: Option<u8>,
    key_count: Option<u8>,
}

fn lightweight_metadata(text: &str) -> LightMetadata {
    let mut out = LightMetadata::default();
    let mut section = String::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_owned();
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match (section.as_str(), key) {
            ("[General]", "Mode") => out.mode = value.parse::<u8>().ok(),
            ("[Difficulty]", "CircleSize") => {
                out.key_count = value
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .map(|value| value.round() as u8);
            }
            _ => {}
        }
    }
    out
}

/// Persistable key-pattern record used by dataset files and consumers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManiaMmaRecord {
    pub beatmap_id: u64,
    pub beatmapset_id: u64,
    pub game_mod: ManiaGameMod,
    pub key_count: u8,
    /// `RC` / `HB` / `Mix` / `LN`.
    pub mode_tag: String,
    /// Category, for example `Shield` or `Jumpstream/Handstream Tech`.
    pub category: String,
    /// Core category of the highest-importance cluster.
    pub dominant_pattern: String,
    pub ln_note_ratio: f32,
    pub hb_row_ratio: f32,
    pub sv_amount: f32,
    pub duration_seconds: f32,
    /// Coverage of the six categories, ordered Stream, Chordstream, Jacks, Coordination, Density, Wildcard.
    pub coverage: [f32; 6],
    pub clusters: Vec<ManiaMmaCluster>,
    pub bars: Vec<ManiaMmaBar>,
    pub subtypes: Vec<(String, f32)>,
    /// Average, peak (95th percentile) and sustained (median) NPS, plus the longest sustained run in seconds.
    pub intensity: [f32; 4],
    /// Sustained share, longest sustained run as a share of the chart, gap share.
    pub temporal: [f32; 3],
    /// Mean change of every pattern between adjacent time windows.
    pub variation: [f32; 6],
    pub note_count: u32,
    pub row_count: u32,
    pub avg_nps: f32,
    pub peak_nps: f32,
    pub chord_rate: f32,
    pub large_chord_rate: f32,
    pub unclassified: f32,
    pub algorithm_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManiaMmaCluster {
    pub pattern: String,
    pub specific_types: Vec<(String, f32)>,
    pub rating_multiplier: f32,
    pub bpm: i32,
    pub mixed: bool,
    pub amount: f32,
    pub importance: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManiaMmaBar {
    pub pattern: String,
    pub amount: f32,
    pub bpm: f32,
    pub relative: f32,
    pub specific_types: Vec<(String, f32)>,
}

impl ManiaMmaAnalysis {
    pub fn into_record(
        self,
        beatmap_id: u64,
        beatmapset_id: u64,
        game_mod: ManiaGameMod,
    ) -> ManiaMmaRecord {
        let dominant = self
            .report
            .clusters
            .first()
            .map(|cluster| cluster.pattern.to_owned())
            .unwrap_or_default();

        ManiaMmaRecord {
            beatmap_id,
            beatmapset_id,
            game_mod,
            key_count: self.key_count,
            mode_tag: self.report.mode_tag.to_owned(),
            category: self.report.category.clone(),
            dominant_pattern: dominant,
            ln_note_ratio: self.report.ln_percent as f32,
            hb_row_ratio: self.report.hb_row_ratio as f32,
            sv_amount: self.report.sv_amount as f32,
            duration_seconds: (self.report.duration / 1000.0) as f32,
            coverage: self.features.coverage.map(|value| value as f32),
            clusters: self
                .report
                .clusters
                .iter()
                .map(|cluster| ManiaMmaCluster {
                    pattern: cluster.pattern.to_owned(),
                    specific_types: cluster
                        .specific_types
                        .iter()
                        .map(|(name, ratio)| (name.clone(), *ratio as f32))
                        .collect(),
                    rating_multiplier: cluster.rating_multiplier as f32,
                    bpm: cluster.bpm as i32,
                    mixed: cluster.mixed,
                    amount: cluster.amount as f32,
                    importance: cluster.importance as f32,
                })
                .collect(),
            bars: self
                .features
                .bars
                .iter()
                .map(|bar| ManiaMmaBar {
                    pattern: bar.pattern.to_owned(),
                    amount: bar.amount as f32,
                    bpm: bar.bpm as f32,
                    relative: bar.relative as f32,
                    specific_types: bar
                        .specific_types
                        .iter()
                        .map(|(name, ratio)| (name.clone(), *ratio as f32))
                        .collect(),
                })
                .collect(),
            subtypes: self
                .features
                .subtypes
                .iter()
                .map(|(name, ratio)| (name.clone(), *ratio as f32))
                .collect(),
            intensity: self.features.intensity.map(|value| value as f32),
            temporal: self.features.temporal.map(|value| value as f32),
            variation: self.features.variation.map(|value| value as f32),
            note_count: self.features.note_count,
            row_count: self.features.row_count,
            avg_nps: self.features.avg_nps as f32,
            peak_nps: self.features.peak_nps as f32,
            chord_rate: self.features.chord_rate as f32,
            large_chord_rate: self.features.large_chord_rate as f32,
            unclassified: self.features.unclassified as f32,
            algorithm_version: MANIA_MMA_ALGORITHM_VERSION,
        }
    }
}

/// Record format version; bump it whenever the field layout changes.
pub const MANIA_MMA_ALGORITHM_VERSION: u32 = 2;

pub fn analyze_record(
    text: &str,
    beatmap_id: u64,
    beatmapset_id: u64,
    game_mod: ManiaGameMod,
) -> Result<ManiaMmaRecord> {
    Ok(analyze(text, game_mod)?.into_record(beatmap_id, beatmapset_id, game_mod))
}
pub fn validate_record(record: &ManiaMmaRecord) -> Result<()> {
    if record.algorithm_version != MANIA_MMA_ALGORITHM_VERSION {
        bail!("mania key-pattern record has the wrong algorithm version");
    }
    if !matches!(record.key_count, 4 | 6 | 7) {
        bail!("mania key-pattern record has an unsupported key count");
    }
    if !matches!(record.mode_tag.as_str(), "RC" | "HB" | "Mix" | "LN") {
        bail!("mania key-pattern record has an unknown mode tag");
    }
    if record.category.is_empty() {
        bail!("mania key-pattern record has an empty category");
    }
    if record
        .coverage
        .into_iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        bail!("mania key-pattern coverage must stay within 0..=1");
    }
    if !(0.0..=1.0).contains(&record.ln_note_ratio)
        || !(0.0..=1.0).contains(&record.hb_row_ratio)
        || !record.sv_amount.is_finite()
        || record.sv_amount < 0.0
        || !record.duration_seconds.is_finite()
        || record.duration_seconds <= 0.0
    {
        bail!("mania key-pattern record has a non-finite or negative summary value");
    }
    if record
        .intensity
        .into_iter()
        .chain(record.temporal)
        .chain(record.variation)
        .chain([
            record.avg_nps,
            record.peak_nps,
            record.chord_rate,
            record.large_chord_rate,
            record.unclassified,
        ])
        .any(|value| !value.is_finite())
    {
        bail!("mania key-pattern record has a non-finite derived value");
    }
    if record
        .subtypes
        .iter()
        .chain(record.bars.iter().flat_map(|bar| bar.specific_types.iter()))
        .any(|(_, ratio)| !ratio.is_finite() || !(0.0..=1.0).contains(ratio))
    {
        bail!("mania key-pattern subtype ratio must stay within 0..=1");
    }
    if record.clusters.iter().any(|cluster| {
        !cluster.amount.is_finite()
            || !cluster.importance.is_finite()
            || !cluster.rating_multiplier.is_finite()
            || !matches!(
                cluster.pattern.as_str(),
                "Stream" | "Chordstream" | "Jacks" | "Coordination" | "Density" | "Wildcard"
            )
    }) {
        bail!("mania key-pattern cluster is malformed");
    }
    Ok(())
}
