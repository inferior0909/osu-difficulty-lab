//! osu!mania key-pattern analysis, ported from osumania_map_analyser v3.0.0.
//!
//! Module layout mirrors the original files:
//!
//! | osumania_map_analyser | here |
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
pub use features::KeyPatternFeatures;
pub use rate::ManiaGameMod;
pub use summary::KeyPatternReport;

/// The analyser version these rules are copied from.
pub const KEY_PATTERN_REFERENCE: &str =
    "osumania_map_analyser v3.0.0 (2fb795e5e205c50c7bcb35b370379922c830e72b)";

/// Name of the rule set. Change it whenever rules, parameters or derived features change.
pub const KEY_PATTERN_RULESET: &str = "mania-key-pattern-1.0";

/// Byte layout of [`KeyPatternRecord`]. Locked at 2: released OPP only reads records with this value.
/// New fields go at the end of the record under the same number (released OPP ignores trailing bytes);
/// only changing or removing an existing field needs a new number.
pub const KEY_PATTERN_RECORD_FORMAT: u32 = 2;

// Old names, kept so OPP 0.5.0+ can move to this version without renaming anything.
pub type ManiaMmaRecord = KeyPatternRecord;
pub type ManiaMmaCluster = KeyPatternCluster;
pub type ManiaMmaBar = KeyPatternBar;
pub const MANIA_MMA_ALGORITHM_VERSION: u32 = KEY_PATTERN_RECORD_FORMAT;

#[derive(Debug, Clone)]
pub struct KeyPatternAnalysis {
    pub key_count: u8,
    pub report: KeyPatternReport,
    pub features: KeyPatternFeatures,
}

/// Analyses one chart at the clock rate of `game_mod`.
///
/// As in osumania_map_analyser v3.0.0, patterns are always found at the original speed; DT/HT only
/// rescale times and BPM ([`KeyPatternReport::at_rate`]), so mode tag, category and cluster names
/// equal NM. Note density and time structure are measured on the chart stretched to DT/HT timing.
pub fn analyze(text: &str, game_mod: ManiaGameMod) -> Result<KeyPatternAnalysis> {
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

    let chart = parser::parse_osu_mania(text)?;
    // The parser truncates CircleSize while the check above rounds it, so 3.6 would pass as 4K.
    if chart.keys != key_count as usize {
        bail!("CircleSize is not a whole key count");
    }

    let rate = game_mod.rate();
    let report = summary::from_chart(&chart).at_rate(rate);
    let timed_chart = if game_mod == ManiaGameMod::Nm {
        chart
    } else {
        parser::parse_osu_mania(&rate::transform_rate(text, rate)?)?
    };
    let features = features::derive(&timed_chart, &report);

    Ok(KeyPatternAnalysis {
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

/// Persisted key-pattern record, one per beatmap and clock rate.
///
/// Field order is the binary format read by OPP (bincode, fixed-width little endian): never reorder
/// or remove fields, add new ones only at the end. Names are not stored, but OPP code uses them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyPatternRecord {
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
    /// Milliseconds of scroll-speed changes, always measured at the original speed.
    pub sv_amount: f32,
    pub duration_seconds: f32,
    /// Coverage of the six categories, ordered Stream, Chordstream, Jacks, Coordination, Density, Wildcard.
    pub coverage: [f32; 6],
    pub clusters: Vec<KeyPatternCluster>,
    pub bars: Vec<KeyPatternBar>,
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
    /// Always [`KEY_PATTERN_RECORD_FORMAT`].
    pub algorithm_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyPatternCluster {
    pub pattern: String,
    pub specific_types: Vec<(String, f32)>,
    pub rating_multiplier: f32,
    /// BPM at this record's clock rate.
    pub bpm: i32,
    pub mixed: bool,
    pub amount: f32,
    pub importance: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyPatternBar {
    pub pattern: String,
    pub amount: f32,
    pub bpm: f32,
    pub relative: f32,
    pub specific_types: Vec<(String, f32)>,
}

fn to_f32_pairs(pairs: &[(String, f64)]) -> Vec<(String, f32)> {
    pairs
        .iter()
        .map(|(name, ratio)| (name.clone(), *ratio as f32))
        .collect()
}

impl KeyPatternAnalysis {
    pub fn into_record(
        self,
        beatmap_id: u64,
        beatmapset_id: u64,
        game_mod: ManiaGameMod,
    ) -> KeyPatternRecord {
        let report = self.report;
        let features = self.features;
        KeyPatternRecord {
            beatmap_id,
            beatmapset_id,
            game_mod,
            key_count: self.key_count,
            mode_tag: report.mode_tag.to_owned(),
            category: report.category,
            dominant_pattern: report
                .clusters
                .first()
                .map(|cluster| cluster.pattern.to_owned())
                .unwrap_or_default(),
            ln_note_ratio: report.ln_percent as f32,
            hb_row_ratio: report.hb_row_ratio as f32,
            // v3.0.0 measures SV time from the first note, so speed changes placed before it make
            // the total negative (e.g. Ranked map 1940322 gives -921501 ms). Stored as 0, which
            // keeps the same "not an SV chart" verdict and passes OPP's non-negative check.
            sv_amount: report.sv_amount.max(0.0) as f32,
            duration_seconds: (report.duration / 1000.0) as f32,
            coverage: features.coverage.map(|value| value as f32),
            clusters: report
                .clusters
                .iter()
                .map(|cluster| KeyPatternCluster {
                    pattern: cluster.pattern.to_owned(),
                    specific_types: to_f32_pairs(&cluster.specific_types),
                    rating_multiplier: cluster.rating_multiplier as f32,
                    bpm: cluster.bpm as i32,
                    mixed: cluster.mixed,
                    amount: cluster.amount as f32,
                    importance: cluster.importance as f32,
                })
                .collect(),
            bars: features
                .bars
                .iter()
                .map(|bar| KeyPatternBar {
                    pattern: bar.pattern.to_owned(),
                    amount: bar.amount as f32,
                    bpm: bar.bpm as f32,
                    relative: bar.relative as f32,
                    specific_types: to_f32_pairs(&bar.specific_types),
                })
                .collect(),
            subtypes: to_f32_pairs(&features.subtypes),
            intensity: features.intensity.map(|value| value as f32),
            temporal: features.temporal.map(|value| value as f32),
            variation: features.variation.map(|value| value as f32),
            note_count: features.note_count,
            row_count: features.row_count,
            avg_nps: features.avg_nps as f32,
            peak_nps: features.peak_nps as f32,
            chord_rate: features.chord_rate as f32,
            large_chord_rate: features.large_chord_rate as f32,
            unclassified: features.unclassified as f32,
            algorithm_version: KEY_PATTERN_RECORD_FORMAT,
        }
    }
}

pub fn analyze_record(
    text: &str,
    beatmap_id: u64,
    beatmapset_id: u64,
    game_mod: ManiaGameMod,
) -> Result<KeyPatternRecord> {
    Ok(analyze(text, game_mod)?.into_record(beatmap_id, beatmapset_id, game_mod))
}

pub fn validate_record(record: &KeyPatternRecord) -> Result<()> {
    if record.algorithm_version != KEY_PATTERN_RECORD_FORMAT {
        bail!("mania key-pattern record has the wrong record format");
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
