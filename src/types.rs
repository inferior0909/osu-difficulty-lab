use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct DifficultyVector {
    pub aim: f32,
    pub speed: f32,
    pub reading: f32,
    pub slider: f32,
    pub overlap: f32,
}

impl DifficultyVector {
    pub const fn as_array(self) -> [f32; 5] {
        [
            self.aim,
            self.speed,
            self.reading,
            self.slider,
            self.overlap,
        ]
    }
    pub const fn from_array(value: [f32; 5]) -> Self {
        Self {
            aim: value[0],
            speed: value[1],
            reading: value[2],
            slider: value[3],
            overlap: value[4],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct BaseFeatures {
    pub bpm: f32,
    pub ar: f32,
    pub od: f32,
    pub cs: f32,
    pub hp: f32,
    pub length_seconds: f32,
    pub object_count: f32,
    pub object_density: f32,
    pub circle_ratio: f32,
    pub slider_ratio: f32,
    pub spinner_ratio: f32,
    pub max_combo: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct OverlapStatistics {
    pub peak: f32,
    pub p95: f32,
    pub sustained_ratio: f32,
    pub stack_rate: f32,
    pub slider_occlusion_rate: f32,
    pub path_crossing_rate: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct BeatmapFeatureRecord {
    pub beatmap_id: u64,
    pub beatmapset_id: u64,
    pub difficulty: DifficultyVector,
    pub base: BaseFeatures,
    pub overlap: OverlapStatistics,
    pub analyzer_version: u32,
    pub normalization_version: u32,
    pub mod_profile: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RawFeatureRecord {
    pub beatmap_id: u64,
    pub beatmapset_id: u64,
    pub raw_difficulty: DifficultyVector,
    pub base: BaseFeatures,
    pub overlap: OverlapStatistics,
    pub analyzer_version: u32,
    pub mod_profile: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverlapConfig {
    pub overlap_window_ms: f64,
    pub temporal_tau_ms: f64,
    pub maximum_distance_ratio: f64,
    pub stack_distance_ratio: f64,
    pub circle_weight: f64,
    pub slider_weight: f64,
    pub ambiguity_weight: f64,
    pub stack_weight: f64,
    pub crossing_weight: f64,
    pub strain_decay_base: f64,
    pub section_length_ms: f64,
    pub peak_weight_decay: f64,
}

impl Default for OverlapConfig {
    fn default() -> Self {
        Self {
            overlap_window_ms: 3000.0,
            temporal_tau_ms: 700.0,
            maximum_distance_ratio: 1.5,
            stack_distance_ratio: 0.5,
            circle_weight: 0.35,
            slider_weight: 0.25,
            ambiguity_weight: 0.15,
            stack_weight: 0.15,
            crossing_weight: 0.10,
            strain_decay_base: 0.25,
            section_length_ms: 400.0,
            peak_weight_decay: 0.90,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    pub overlap: OverlapConfig,
    pub reading_section_ms: f64,
    pub reading_peak_weight_decay: f64,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            overlap: OverlapConfig::default(),
            reading_section_ms: 400.0,
            reading_peak_weight_decay: 0.90,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DifficultyWeights {
    pub aim: f32,
    pub speed: f32,
    pub reading: f32,
    pub slider: f32,
    pub overlap: f32,
}
impl Default for DifficultyWeights {
    fn default() -> Self {
        Self {
            aim: 1.0,
            speed: 1.0,
            reading: 1.0,
            slider: 1.0,
            overlap: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaseFeatureWeights {
    pub bpm: f32,
    pub ar: f32,
    pub length_seconds: f32,
    pub object_density: f32,
    pub circle_ratio: f32,
    pub slider_ratio: f32,
}
impl Default for BaseFeatureWeights {
    fn default() -> Self {
        Self {
            bpm: 0.15,
            ar: 0.15,
            length_seconds: 0.10,
            object_density: 0.25,
            circle_ratio: 0.15,
            slider_ratio: 0.20,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryFilters {
    pub min_ar: Option<f32>,
    pub max_ar: Option<f32>,
    pub min_bpm: Option<f32>,
    pub max_bpm: Option<f32>,
    pub beatmapset_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimilarityQuery {
    pub beatmap_id: u64,
    pub mod_profile: u32,
    pub difficulty_weights: DifficultyWeights,
    pub base_weights: BaseFeatureWeights,
    pub filters: QueryFilters,
    pub candidate_limit: usize,
    pub result_limit: usize,
}

impl Default for SimilarityQuery {
    fn default() -> Self {
        Self {
            beatmap_id: 0,
            mod_profile: 0,
            difficulty_weights: DifficultyWeights::default(),
            base_weights: BaseFeatureWeights::default(),
            filters: QueryFilters::default(),
            candidate_limit: 256,
            result_limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimilarityResult {
    pub beatmap_id: u64,
    pub beatmapset_id: u64,
    pub final_distance: f32,
    pub difficulty_distance: f32,
    pub base_distance: f32,
    pub difficulty: DifficultyVector,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeatmapMetadata {
    pub beatmap_id: u64,
    pub beatmapset_id: u64,
    pub checksum: String,
    pub artist: String,
    pub title: String,
    pub version: String,
    pub creator: String,
    pub online_url: String,
}
