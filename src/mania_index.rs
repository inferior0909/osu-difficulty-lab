use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    MANIA_ANALYZER_VERSION, ManiaDistanceComponents, ManiaFeatureRecord, ManiaFeatureStore,
    ManiaModeFamily, ManiaSimilarityQuery, ManiaSimilarityResult,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManiaBucketEntry {
    beatmap_id: u64,
    beatmapset_id: u64,
    mode_family: ManiaModeFamily,
    normalized_offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManiaBucket {
    key_count: u8,
    difficulty_band: u8,
    entries: Vec<ManiaBucketEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManiaBucketIndex {
    normalization_version: u32,
    analyzer_version: u32,
    buckets: Vec<ManiaBucket>,
}

pub fn build_mania_index(store: &ManiaFeatureStore, version: u32) -> Result<()> {
    let records = store.normalized_records_with_offsets(version)?;
    if records.is_empty() {
        bail!("cannot build an empty mania index");
    }
    let mut groups = BTreeMap::<(u8, u8), Vec<(u64, &ManiaFeatureRecord)>>::new();
    for (offset, record) in &records {
        groups
            .entry((record.key_count, record.difficulty_band))
            .or_default()
            .push((*offset, record));
    }
    let buckets = groups
        .into_iter()
        .map(|((key_count, difficulty_band), mut records)| {
            records.sort_by(|(_, left), (_, right)| {
                left.difficulty_percentile
                    .total_cmp(&right.difficulty_percentile)
                    .then_with(|| left.beatmap_id.cmp(&right.beatmap_id))
            });
            ManiaBucket {
                key_count,
                difficulty_band,
                entries: records
                    .into_iter()
                    .map(|(normalized_offset, record)| ManiaBucketEntry {
                        beatmap_id: record.beatmap_id,
                        beatmapset_id: record.beatmapset_id,
                        mode_family: record.mode_family,
                        normalized_offset,
                    })
                    .collect(),
            }
        })
        .collect();
    write_index(
        store.root(),
        version,
        &ManiaBucketIndex {
            normalization_version: version,
            analyzer_version: MANIA_ANALYZER_VERSION,
            buckets,
        },
    )?;
    validate_mania_index_coverage(store, version)
}

pub fn validate_mania_index_coverage(store: &ManiaFeatureStore, version: u32) -> Result<()> {
    let expected = store
        .normalized_records_with_offsets(version)?
        .into_iter()
        .map(|(offset, record)| {
            (
                record.beatmap_id,
                (
                    record.key_count,
                    record.difficulty_band,
                    record.beatmapset_id,
                    record.mode_family,
                    offset,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let index = read_index(store.root(), version)?;
    if index.normalization_version != version || index.analyzer_version != MANIA_ANALYZER_VERSION {
        bail!("mania bucket index version does not match the dataset");
    }
    let mut actual = BTreeMap::new();
    for bucket in index.buckets {
        for entry in bucket.entries {
            if actual
                .insert(
                    entry.beatmap_id,
                    (
                        bucket.key_count,
                        bucket.difficulty_band,
                        entry.beatmapset_id,
                        entry.mode_family,
                        entry.normalized_offset,
                    ),
                )
                .is_some()
            {
                bail!(
                    "mania beatmap {} occurs in more than one bucket",
                    entry.beatmap_id
                );
            }
        }
    }
    if actual != expected {
        bail!("mania bucket index records do not match normalized records");
    }
    Ok(())
}

pub struct ManiaSimilarityStore {
    root: PathBuf,
    version: u32,
    index: ManiaBucketIndex,
}

impl ManiaSimilarityStore {
    pub fn open(root: impl AsRef<Path>, version: u32) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index = read_index(&root, version)?;
        if index.normalization_version != version
            || index.analyzer_version != MANIA_ANALYZER_VERSION
        {
            bail!("mania bucket index version does not match the active dataset");
        }
        Ok(Self {
            root,
            version,
            index,
        })
    }

    pub fn query_by_id(
        &self,
        store: &ManiaFeatureStore,
        beatmap_id: u64,
        query: ManiaSimilarityQuery,
    ) -> Result<Vec<ManiaSimilarityResult>> {
        let target = store.normalized_for_id(self.version, beatmap_id)?;
        self.query_record(store, target, query)
    }

    pub fn query_record(
        &self,
        store: &ManiaFeatureStore,
        target: ManiaFeatureRecord,
        query: ManiaSimilarityQuery,
    ) -> Result<Vec<ManiaSimilarityResult>> {
        if !(1..=100).contains(&query.result_limit) {
            bail!("mania query limit must be between 1 and 100");
        }
        if target.normalization_version != self.version
            || target.analyzer_version != MANIA_ANALYZER_VERSION
        {
            bail!("mania query target version does not match the index");
        }
        let target_total = 256_usize.max(query.result_limit.saturating_mul(4));
        let target_family = 32_usize.max(query.result_limit.saturating_mul(2));
        let mut candidate_entries = BTreeMap::<u64, &ManiaBucketEntry>::new();
        let center = target.difficulty_band as i16;
        for radius in 0_i16..=9 {
            let mut bands = vec![center - radius];
            if radius > 0 {
                bands.push(center + radius);
            }
            for band in bands.into_iter().filter(|band| (0..=9).contains(band)) {
                if let Some(bucket) = self.index.buckets.iter().find(|bucket| {
                    bucket.key_count == target.key_count && bucket.difficulty_band == band as u8
                }) {
                    candidate_entries
                        .extend(bucket.entries.iter().map(|entry| (entry.beatmap_id, entry)));
                }
            }
            let mut total = 0_usize;
            let mut same_family = 0_usize;
            for candidate in candidate_entries.values() {
                if excluded_entry(&target, candidate, query.include_same_set) {
                    continue;
                }
                total += 1;
                if candidate.mode_family == target.mode_family {
                    same_family += 1;
                }
            }
            if total >= target_total && same_family >= target_family {
                break;
            }
        }

        let selected = candidate_entries
            .into_values()
            .filter(|candidate| !excluded_entry(&target, candidate, query.include_same_set))
            .collect::<Vec<_>>();
        let offsets = selected
            .iter()
            .map(|entry| entry.normalized_offset)
            .collect::<Vec<_>>();
        let candidates = store.normalized_records_at_offsets(self.version, &offsets)?;
        let mut ranked = selected
            .into_iter()
            .zip(candidates)
            .map(|(entry, candidate)| {
                if candidate.beatmap_id != entry.beatmap_id
                    || candidate.beatmapset_id != entry.beatmapset_id
                    || candidate.key_count != target.key_count
                    || candidate.mode_family != entry.mode_family
                {
                    bail!(
                        "mania index entry {} does not match its normalized record",
                        entry.beatmap_id
                    );
                }
                let components = distance_components(target, candidate);
                let final_distance = 0.35 * components.skill
                    + 0.30 * components.pattern
                    + 0.20 * components.structure
                    + 0.10 * components.difficulty
                    + 0.05 * components.context;
                Ok((candidate, components, final_distance))
            })
            .collect::<Result<Vec<_>>>()?;
        ranked.sort_by(|left, right| {
            left.2
                .total_cmp(&right.2)
                .then_with(|| left.0.beatmap_id.cmp(&right.0.beatmap_id))
        });
        ranked.truncate(query.result_limit);
        ranked
            .into_iter()
            .map(|(candidate, components, final_distance)| {
                let metadata = store.metadata_for(candidate.beatmap_id)?;
                Ok(ManiaSimilarityResult {
                    beatmap_id: candidate.beatmap_id,
                    beatmapset_id: candidate.beatmapset_id,
                    artist: metadata.artist,
                    title: metadata.title,
                    version: metadata.version,
                    key_count: candidate.key_count,
                    mode_family: candidate.mode_family,
                    dominant_pattern: candidate.dominant_pattern,
                    difficulty_percentile: candidate.difficulty_percentile,
                    difficulty_band: candidate.difficulty_band,
                    final_distance,
                    components,
                })
            })
            .collect()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn excluded_entry(
    target: &ManiaFeatureRecord,
    candidate: &ManiaBucketEntry,
    include_same_set: bool,
) -> bool {
    candidate.beatmap_id == target.beatmap_id
        || (!include_same_set
            && target.beatmapset_id != 0
            && candidate.beatmapset_id == target.beatmapset_id)
}

fn distance_components(
    target: ManiaFeatureRecord,
    candidate: ManiaFeatureRecord,
) -> ManiaDistanceComponents {
    ManiaDistanceComponents {
        skill: hellinger(
            &target.difficulty.as_array(),
            &candidate.difficulty.as_array(),
        ),
        pattern: hellinger(
            &target.style.pattern_array(),
            &candidate.style.pattern_array(),
        ),
        structure: rms_distance(
            &target.style.structure_array(),
            &candidate.style.structure_array(),
        ),
        difficulty: (target.difficulty_percentile - candidate.difficulty_percentile).abs(),
        context: 0.5 * log_ratio_distance(target.base.bpm, candidate.base.bpm, 4.0)
            + 0.5
                * log_ratio_distance(
                    target.base.active_length_seconds,
                    candidate.base.active_length_seconds,
                    10.0,
                ),
    }
}

fn hellinger<const N: usize>(left: &[f32; N], right: &[f32; N]) -> f32 {
    const EPSILON: f32 = 1e-6;
    let left_sum = left.iter().sum::<f32>() + EPSILON * N as f32;
    let right_sum = right.iter().sum::<f32>() + EPSILON * N as f32;
    (left
        .iter()
        .zip(right)
        .map(|(left, right)| {
            let left = ((*left + EPSILON) / left_sum).sqrt();
            let right = ((*right + EPSILON) / right_sum).sqrt();
            (left - right).powi(2)
        })
        .sum::<f32>()
        / 2.0)
        .sqrt()
        .clamp(0.0, 1.0)
}

fn rms_distance<const N: usize>(left: &[f32; N], right: &[f32; N]) -> f32 {
    (left
        .iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f32>()
        / N as f32)
        .sqrt()
        .clamp(0.0, 1.0)
}

fn log_ratio_distance(left: f32, right: f32, maximum_ratio: f32) -> f32 {
    (((left.max(0.0) + 1.0) / (right.max(0.0) + 1.0)).ln().abs() / maximum_ratio.ln())
        .clamp(0.0, 1.0)
}

fn index_path(root: &Path, version: u32) -> PathBuf {
    root.join("indexes")
        .join(format!("mania-v{version}.buckets"))
}

fn checksum_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", path.display()))
}

fn write_index(root: &Path, version: u32, index: &ManiaBucketIndex) -> Result<()> {
    let path = index_path(root, version);
    let temporary = path.with_extension("buckets.tmp");
    let bytes = bincode::serialize(index)?;
    fs::write(&temporary, &bytes)?;
    let checksum = hex::encode(Sha256::digest(&bytes));
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(&temporary, &path)?;
    fs::write(checksum_path(&path), checksum)?;
    Ok(())
}

fn read_index(root: &Path, version: u32) -> Result<ManiaBucketIndex> {
    let path = index_path(root, version);
    let bytes = fs::read(&path)
        .with_context(|| format!("mania bucket index is missing: {}", path.display()))?;
    let checksum_file = checksum_path(&path);
    let saved = fs::read_to_string(&checksum_file).with_context(|| {
        format!(
            "mania index checksum is missing: {}",
            checksum_file.display()
        )
    })?;
    if saved.trim() != hex::encode(Sha256::digest(&bytes)) {
        bail!("mania bucket index checksum mismatch: {}", path.display());
    }
    bincode::deserialize(&bytes)
        .with_context(|| format!("invalid mania bucket index: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{
        MANIA_ANALYZER_VERSION, ManiaBeatmapMetadata, ManiaDifficultyVector, ManiaModeFamily,
        ManiaPattern, ManiaRawFeatureRecord, ManiaStyleVector, fit_mania_normalizer,
    };

    fn add(
        store: &mut ManiaFeatureStore,
        id: u64,
        set: u64,
        value: f32,
        family: ManiaModeFamily,
    ) -> Result<()> {
        let pattern = if family == ManiaModeFamily::Rc {
            ManiaPattern::Stream
        } else {
            ManiaPattern::Wildcard
        };
        let metadata = ManiaBeatmapMetadata {
            beatmap_id: id,
            beatmapset_id: set,
            checksum: format!("{id}"),
            artist: "artist".into(),
            title: "title".into(),
            version: format!("v{id}"),
            creator: "creator".into(),
            online_url: String::new(),
            key_count: 4,
            mode_family: family,
            dominant_pattern: pattern,
        };
        let mut style = ManiaStyleVector::default();
        if family == ManiaModeFamily::Rc {
            style.stream = 1.0;
        } else {
            style.wildcard = 1.0;
            style.ln_note_ratio = 1.0;
        }
        store.append_raw(
            &metadata,
            &ManiaRawFeatureRecord {
                beatmap_id: id,
                beatmapset_id: set,
                difficulty: ManiaDifficultyVector::from_array([value; 8]),
                style,
                key_count: 4,
                mode_family: family,
                dominant_pattern: pattern,
                analyzer_version: MANIA_ANALYZER_VERSION,
                ..ManiaRawFeatureRecord::default()
            },
        )?;
        Ok(())
    }

    #[test]
    fn query_expands_sparse_bands_and_excludes_same_set() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 1, 1.0, ManiaModeFamily::Ln)?;
        add(&mut store, 11, 1, 1.1, ManiaModeFamily::Ln)?;
        add(&mut store, 20, 2, 5.0, ManiaModeFamily::Ln)?;
        add(&mut store, 30, 3, 9.0, ManiaModeFamily::Rc)?;
        fit_mania_normalizer(&mut store, 1)?;
        build_mania_index(&store, 1)?;
        let results = ManiaSimilarityStore::open(temp.path(), 1)?.query_by_id(
            &store,
            10,
            ManiaSimilarityQuery {
                result_limit: 2,
                include_same_set: false,
            },
        )?;
        assert!(results.iter().all(|result| result.beatmapset_id != 1));
        assert_eq!(results.first().map(|result| result.beatmap_id), Some(20));
        let with_same_set = ManiaSimilarityStore::open(temp.path(), 1)?.query_by_id(
            &store,
            10,
            ManiaSimilarityQuery {
                result_limit: 2,
                include_same_set: true,
            },
        )?;
        assert_eq!(
            with_same_set.first().map(|result| result.beatmap_id),
            Some(11)
        );
        validate_mania_index_coverage(&store, 1)?;
        Ok(())
    }

    #[test]
    fn equal_distances_are_stably_sorted_by_beatmap_id() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 1, 1.0, ManiaModeFamily::Rc)?;
        add(&mut store, 30, 3, 1.0, ManiaModeFamily::Rc)?;
        add(&mut store, 20, 2, 1.0, ManiaModeFamily::Rc)?;
        fit_mania_normalizer(&mut store, 1)?;
        build_mania_index(&store, 1)?;
        let results = ManiaSimilarityStore::open(temp.path(), 1)?.query_by_id(
            &store,
            10,
            ManiaSimilarityQuery {
                result_limit: 2,
                include_same_set: false,
            },
        )?;
        assert_eq!(
            results
                .iter()
                .map(|result| result.beatmap_id)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
        assert!(results.iter().all(|result| {
            result.final_distance == 0.0
                && result.components.skill == 0.0
                && result.components.pattern == 0.0
                && result.components.structure == 0.0
                && result.components.difficulty == 0.0
                && result.components.context == 0.0
        }));
        Ok(())
    }

    #[test]
    fn corrupted_index_fails_checksum_validation() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        add(&mut store, 10, 1, 1.0, ManiaModeFamily::Rc)?;
        build_mania_index_after_fit(&mut store)?;
        fs::write(index_path(temp.path(), 1), b"corrupt")?;
        assert!(ManiaSimilarityStore::open(temp.path(), 1).is_err());
        Ok(())
    }

    fn build_mania_index_after_fit(store: &mut ManiaFeatureStore) -> Result<()> {
        fit_mania_normalizer(store, 1)?;
        build_mania_index(store, 1)
    }
}
