use std::{fs::File, path::Path};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{BeatmapFeatureRecord, DifficultyVector, FeatureStore, RawFeatureRecord};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Normalizer {
    pub version: u32,
    pub quantiles: [Vec<f32>; 5],
}

impl Normalizer {
    pub fn transform(&self, raw: DifficultyVector) -> DifficultyVector {
        let values = raw.as_array();
        let mut normalized = [0.0; 5];
        for (index, value) in values.into_iter().enumerate() {
            normalized[index] = rank(&self.quantiles[index], value);
        }
        DifficultyVector::from_array(normalized)
    }
    pub fn save(&self, root: impl AsRef<Path>) -> Result<()> {
        let path = root
            .as_ref()
            .join("normalizers")
            .join(format!("v{}.bin", self.version));
        bincode::serialize_into(File::create(path)?, self)?;
        Ok(())
    }
    pub fn load(root: impl AsRef<Path>, version: u32) -> Result<Self> {
        Ok(bincode::deserialize_from(File::open(
            root.as_ref()
                .join("normalizers")
                .join(format!("v{version}.bin")),
        )?)?)
    }
}

pub fn fit_normalizer(store: &mut FeatureStore, version: u32) -> Result<Normalizer> {
    if version == 0 {
        bail!("normalization version must be positive");
    }
    let raw = store.raw_records()?;
    if raw.is_empty() {
        bail!("cannot fit a normalizer without raw feature records");
    }
    let mut columns: [Vec<f32>; 5] = std::array::from_fn(|_| Vec::with_capacity(raw.len()));
    for record in &raw {
        for (index, value) in record.raw_difficulty.as_array().into_iter().enumerate() {
            columns[index].push(value);
        }
    }
    for values in &mut columns {
        values.sort_by(f32::total_cmp);
        values.dedup_by(|left, right| left.total_cmp(right).is_eq());
    }
    let normalizer = Normalizer {
        version,
        quantiles: columns,
    };
    normalizer.save(store.root())?;
    let normalized = raw
        .iter()
        .map(|record| normalized_record(record, &normalizer))
        .collect::<Vec<_>>();
    store.write_normalized(version, &normalized)?;
    Ok(normalizer)
}

fn normalized_record(raw: &RawFeatureRecord, normalizer: &Normalizer) -> BeatmapFeatureRecord {
    BeatmapFeatureRecord {
        beatmap_id: raw.beatmap_id,
        beatmapset_id: raw.beatmapset_id,
        difficulty: normalizer.transform(raw.raw_difficulty),
        base: raw.base,
        overlap: raw.overlap,
        analyzer_version: raw.analyzer_version,
        normalization_version: normalizer.version,
        mod_profile: raw.mod_profile,
        flags: 0,
    }
}

fn rank(values: &[f32], value: f32) -> f32 {
    if values.len() <= 1 {
        return 0.0;
    }
    let index = values.partition_point(|candidate| *candidate <= value);
    (index.saturating_sub(1) as f32 / (values.len() - 1) as f32).clamp(0.0, 1.0)
}
