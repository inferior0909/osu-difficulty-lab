use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::Path,
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    MANIA_ANALYZER_VERSION, MANIA_DIFFICULTY_DIMENSIONS, ManiaDifficultyVector, ManiaFeatureRecord,
    ManiaFeatureStore, ManiaRawFeatureRecord,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyNormalizer {
    key_count: u8,
    axes: [Vec<f32>; MANIA_DIFFICULTY_DIMENSIONS],
    overall: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManiaNormalizer {
    pub version: u32,
    pub analyzer_version: u32,
    keys: Vec<KeyNormalizer>,
}

impl ManiaNormalizer {
    pub fn transform(&self, raw: &ManiaRawFeatureRecord) -> Result<ManiaFeatureRecord> {
        if self.analyzer_version != MANIA_ANALYZER_VERSION
            || raw.analyzer_version != self.analyzer_version
        {
            bail!("mania raw feature version does not match the normalizer");
        }
        if raw
            .difficulty
            .as_array()
            .into_iter()
            .any(|value| !value.is_finite() || value < 0.0)
            || raw
                .style
                .as_array()
                .into_iter()
                .any(|value| !(0.0..=1.0).contains(&value))
        {
            bail!("mania raw feature is outside its declared range");
        }
        let key = self
            .keys
            .iter()
            .find(|normalizer| normalizer.key_count == raw.key_count)
            .ok_or_else(|| anyhow::anyhow!("normalizer has no {}K cohort", raw.key_count))?;
        let mut normalized = [0.0_f32; MANIA_DIFFICULTY_DIMENSIONS];
        for (index, value) in raw.difficulty.as_array().into_iter().enumerate() {
            normalized[index] = midrank(&key.axes[index], value);
        }
        let overall = overall_intensity(normalized);
        let percentile = midrank(&key.overall, overall);
        Ok(ManiaFeatureRecord {
            beatmap_id: raw.beatmap_id,
            beatmapset_id: raw.beatmapset_id,
            difficulty: ManiaDifficultyVector::from_array(normalized),
            style: raw.style,
            base: raw.base,
            difficulty_percentile: percentile,
            difficulty_band: ((percentile * 10.0).floor() as u8).min(9),
            key_count: raw.key_count,
            mode_family: raw.mode_family,
            dominant_pattern: raw.dominant_pattern,
            analyzer_version: raw.analyzer_version,
            normalization_version: self.version,
        })
    }

    pub fn save(&self, root: impl AsRef<Path>) -> Result<()> {
        let path = root
            .as_ref()
            .join("normalizers")
            .join(format!("mania-v{}.bin", self.version));
        let temporary = path.with_extension("bin.tmp");
        bincode::serialize_into(File::create(&temporary)?, self)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn load(root: impl AsRef<Path>, version: u32) -> Result<Self> {
        let normalizer: Self = bincode::deserialize_from(File::open(
            root.as_ref()
                .join("normalizers")
                .join(format!("mania-v{version}.bin")),
        )?)?;
        if normalizer.version != version || normalizer.analyzer_version != MANIA_ANALYZER_VERSION {
            bail!("mania normalizer version does not match the active analyzer");
        }
        Ok(normalizer)
    }
}

pub fn fit_mania_normalizer(
    store: &mut ManiaFeatureStore,
    version: u32,
) -> Result<ManiaNormalizer> {
    if version == 0 {
        bail!("mania normalization version must be positive");
    }
    let raw = store.raw_records()?;
    if raw.is_empty() {
        bail!("cannot fit a mania normalizer without raw records");
    }
    let mut cohorts = BTreeMap::<u8, Vec<&ManiaRawFeatureRecord>>::new();
    for record in &raw {
        cohorts.entry(record.key_count).or_default().push(record);
    }
    let mut keys = Vec::new();
    for (key_count, records) in cohorts {
        let mut axes: [Vec<f32>; MANIA_DIFFICULTY_DIMENSIONS] =
            std::array::from_fn(|_| Vec::with_capacity(records.len()));
        for record in &records {
            for (index, value) in record.difficulty.as_array().into_iter().enumerate() {
                axes[index].push(value);
            }
        }
        for values in &mut axes {
            values.sort_by(f32::total_cmp);
        }
        let mut overall = records
            .iter()
            .map(|record| {
                let mut normalized = [0.0; MANIA_DIFFICULTY_DIMENSIONS];
                for (index, value) in record.difficulty.as_array().into_iter().enumerate() {
                    normalized[index] = midrank(&axes[index], value);
                }
                overall_intensity(normalized)
            })
            .collect::<Vec<_>>();
        overall.sort_by(f32::total_cmp);
        keys.push(KeyNormalizer {
            key_count,
            axes,
            overall,
        });
    }
    let normalizer = ManiaNormalizer {
        version,
        analyzer_version: MANIA_ANALYZER_VERSION,
        keys,
    };
    let normalized = raw
        .iter()
        .map(|record| normalizer.transform(record))
        .collect::<Result<Vec<_>>>()?;
    normalizer.save(store.root())?;
    store.write_normalized(version, &normalized)?;
    Ok(normalizer)
}

pub fn overall_intensity(values: [f32; MANIA_DIFFICULTY_DIMENSIONS]) -> f32 {
    let maximum = values.into_iter().fold(0.0_f32, f32::max);
    let rms = (values.into_iter().map(|value| value * value).sum::<f32>()
        / MANIA_DIFFICULTY_DIMENSIONS as f32)
        .sqrt();
    let mut sorted = values;
    sorted.sort_by(|left, right| right.total_cmp(left));
    let top_three = (sorted[0] + sorted[1] + sorted[2]) / 3.0;
    (0.50 * maximum + 0.30 * rms + 0.20 * top_three).clamp(0.0, 1.0)
}

fn midrank(sorted: &[f32], value: f32) -> f32 {
    if sorted.len() <= 1 {
        return 0.5;
    }
    let lower = sorted.partition_point(|candidate| candidate.total_cmp(&value).is_lt());
    let upper = sorted.partition_point(|candidate| !candidate.total_cmp(&value).is_gt());
    let midpoint = if upper > lower {
        (lower + upper - 1) as f32 / 2.0
    } else {
        lower as f32
    };
    (midpoint / (sorted.len() - 1) as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{ManiaBeatmapMetadata, ManiaModeFamily, ManiaPattern, ManiaStyleVector};

    fn add(store: &mut ManiaFeatureStore, id: u64, keys: u8, value: f32) -> Result<()> {
        let metadata = ManiaBeatmapMetadata {
            beatmap_id: id,
            beatmapset_id: id / 10,
            checksum: format!("{id}"),
            artist: "artist".into(),
            title: "title".into(),
            version: "version".into(),
            creator: "creator".into(),
            online_url: String::new(),
            key_count: keys,
            mode_family: ManiaModeFamily::Rc,
            dominant_pattern: ManiaPattern::Stream,
        };
        store.append_raw(
            &metadata,
            &ManiaRawFeatureRecord {
                beatmap_id: id,
                beatmapset_id: id / 10,
                difficulty: ManiaDifficultyVector::from_array([value; 8]),
                style: ManiaStyleVector::default(),
                key_count: keys,
                analyzer_version: MANIA_ANALYZER_VERSION,
                ..ManiaRawFeatureRecord::default()
            },
        )?;
        Ok(())
    }

    #[test]
    fn normalization_is_independent_per_key_count() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 4, 1.0)?;
        add(&mut store, 20, 4, 2.0)?;
        add(&mut store, 60, 6, 100.0)?;
        add(&mut store, 70, 6, 200.0)?;
        fit_mania_normalizer(&mut store, 1)?;
        assert_eq!(store.normalized_for_id(1, 10)?.difficulty.speed, 0.0);
        assert_eq!(store.normalized_for_id(1, 20)?.difficulty.speed, 1.0);
        assert_eq!(store.normalized_for_id(1, 60)?.difficulty.speed, 0.0);
        assert_eq!(store.normalized_for_id(1, 70)?.difficulty.speed, 1.0);
        assert_eq!(store.normalized_for_id(1, 10)?.difficulty_band, 0);
        assert_eq!(store.normalized_for_id(1, 20)?.difficulty_band, 9);
        Ok(())
    }

    #[test]
    fn duplicate_values_receive_the_same_midrank() {
        assert_eq!(midrank(&[1.0, 1.0, 2.0], 1.0), 0.25);
    }

    #[test]
    fn transform_rejects_analyzer_version_mismatch() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 4, 1.0)?;
        let normalizer = fit_mania_normalizer(&mut store, 1)?;
        let mut raw = store.raw_records()?[0];
        raw.analyzer_version += 1;
        assert!(normalizer.transform(&raw).is_err());
        Ok(())
    }

    #[test]
    fn rebuilding_the_same_corpus_is_deterministic() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 4, 1.0)?;
        add(&mut store, 20, 4, 2.0)?;
        fit_mania_normalizer(&mut store, 1)?;
        let first = fs::read(temp.path().join("normalizers/mania-v1.bin"))?;
        let first_records = store.normalized_records(1)?;
        fit_mania_normalizer(&mut store, 1)?;
        assert_eq!(
            first,
            fs::read(temp.path().join("normalizers/mania-v1.bin"))?
        );
        assert_eq!(first_records, store.normalized_records(1)?);
        Ok(())
    }
}
