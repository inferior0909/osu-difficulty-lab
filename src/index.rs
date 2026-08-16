use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use hnsw::{Hnsw, Params, Searcher};
use rand_pcg::Pcg64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use space::{Metric, Neighbor};

use crate::{
    BaseFeatures, DifficultyVector, FeatureStore, QueryFilters, SimilarityQuery, SimilarityResult,
};

type Graph = Hnsw<WeightedL2, [f32; 5], Pcg64, 16, 32>;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct WeightedL2;
impl Metric<[f32; 5]> for WeightedL2 {
    type Unit = u64;
    fn distance(&self, left: &[f32; 5], right: &[f32; 5]) -> Self::Unit {
        left.iter()
            .zip(right)
            .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
            .sum::<f64>()
            .to_bits()
    }
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    labels: Vec<u64>,
    graph: Graph,
    normalization_version: u32,
}

pub fn build_main_index(store: &FeatureStore, normalization_version: u32) -> Result<()> {
    store.validate_star_section_stats(normalization_version)?;
    let records = store.normalized_records(normalization_version)?;
    let mut graph = Graph::new_params(WeightedL2, Params::new().ef_construction(200));
    let mut searcher = Searcher::new();
    let mut labels = Vec::with_capacity(records.len());
    for record in records {
        let index = graph.insert(record.difficulty.as_array(), &mut searcher);
        if index != labels.len() {
            anyhow::bail!("HNSW label order is not contiguous");
        }
        labels.push(record.beatmap_id);
    }
    write_index(
        store.root(),
        "difficulty-main.hnsw",
        &IndexFile {
            labels,
            graph,
            normalization_version,
        },
    )?;
    // A full main-index rebuild absorbs all active records, so the old delta must
    // be replaced instead of retained (which would duplicate or mix versions).
    let delta = IndexFile {
        labels: Vec::new(),
        graph: Graph::new_params(WeightedL2, Params::new().ef_construction(200)),
        normalization_version,
    };
    write_index(store.root(), "difficulty-delta.hnsw", &delta)?;
    Ok(())
}

pub fn validate_index_coverage(store: &FeatureStore, normalization_version: u32) -> Result<()> {
    let expected = store
        .normalized_records(normalization_version)?
        .into_iter()
        .map(|record| record.beatmap_id)
        .collect::<HashSet<_>>();
    let mut indexed = HashSet::new();
    for name in ["difficulty-main.hnsw", "difficulty-delta.hnsw"] {
        let path = store.root().join("indexes").join(name);
        if !path.exists() {
            anyhow::bail!("index is missing: {}", path.display());
        }
        let index = read_index(&path)?;
        if index.normalization_version != normalization_version {
            anyhow::bail!("index normalization version does not match dataset");
        }
        for id in index.labels {
            if !indexed.insert(id) {
                anyhow::bail!("beatmap {id} occurs in both main/delta index data");
            }
        }
    }
    if indexed != expected {
        anyhow::bail!("main/delta index records do not match active normalized records");
    }
    Ok(())
}

pub struct SimilarityStore {
    root: PathBuf,
    normalization_version: u32,
}
impl SimilarityStore {
    pub fn open(root: impl AsRef<Path>, normalization_version: u32) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.join("indexes/difficulty-main.hnsw").exists() {
            anyhow::bail!("main index is missing; run index build first");
        }
        Ok(Self {
            root,
            normalization_version,
        })
    }
    pub fn query(
        &self,
        store: &FeatureStore,
        query: SimilarityQuery,
    ) -> Result<Vec<SimilarityResult>> {
        if query.mod_profile != 0 {
            anyhow::bail!("only NoMod (mod_profile=0) is supported");
        }
        let target = store.normalized_for_id(self.normalization_version, query.beatmap_id)?;
        let mut ids = HashSet::new();
        for name in ["difficulty-main.hnsw", "difficulty-delta.hnsw"] {
            let path = self.root.join("indexes").join(name);
            if path.exists() {
                let index = read_index(&path)?;
                if index.normalization_version != self.normalization_version {
                    anyhow::bail!("index normalization version does not match query");
                }
                for id in candidates(&index, target.difficulty.as_array(), query.candidate_limit) {
                    ids.insert(id);
                }
            }
        }
        let mut results = Vec::new();
        for id in ids {
            if id == target.beatmap_id {
                continue;
            }
            let candidate = store.normalized_for_id(self.normalization_version, id)?;
            if !matches_filters(candidate.base, candidate.beatmapset_id, &query.filters) {
                continue;
            }
            let d1 = difficulty_distance(
                target.difficulty,
                candidate.difficulty,
                query.difficulty_weights,
            );
            let d2 = base_distance(target.base, candidate.base, query.base_weights);
            results.push(SimilarityResult {
                beatmap_id: id,
                beatmapset_id: candidate.beatmapset_id,
                final_distance: 0.8 * d1 + 0.2 * d2,
                difficulty_distance: d1,
                base_distance: d2,
                difficulty: candidate.difficulty,
            });
        }
        results.sort_by(|left, right| {
            left.final_distance
                .total_cmp(&right.final_distance)
                .then_with(|| left.beatmap_id.cmp(&right.beatmap_id))
        });
        results.truncate(query.result_limit);
        Ok(results)
    }
}

fn candidates(index: &IndexFile, vector: [f32; 5], limit: usize) -> Vec<u64> {
    if index.labels.is_empty() {
        return Vec::new();
    }
    let mut searcher = Searcher::new();
    let mut destination = vec![
        Neighbor {
            index: usize::MAX,
            distance: u64::MAX
        };
        limit.min(index.labels.len()).max(1)
    ];
    index
        .graph
        .nearest(
            &vector,
            limit.clamp(64, 128),
            &mut searcher,
            &mut destination,
        )
        .iter()
        .filter_map(|neighbor| index.labels.get(neighbor.index).copied())
        .collect()
}
fn difficulty_distance(
    a: DifficultyVector,
    b: DifficultyVector,
    w: crate::DifficultyWeights,
) -> f32 {
    let x = a.as_array();
    let y = b.as_array();
    let z = w;
    let weights = [z.aim, z.speed, z.reading, z.slider, z.overlap];
    x.iter()
        .zip(y)
        .zip(weights)
        .map(|((a, b), w)| w * (a - b).powi(2))
        .sum::<f32>()
        .sqrt()
}
fn robust(a: f32, b: f32, scale: f32) -> f32 {
    ((a - b).abs() / scale).min(1.0)
}
fn base_distance(a: BaseFeatures, b: BaseFeatures, w: crate::BaseFeatureWeights) -> f32 {
    w.bpm * robust(a.bpm, b.bpm, 300.0)
        + w.ar * robust(a.ar, b.ar, 10.0)
        + w.length_seconds * robust(a.length_seconds, b.length_seconds, 300.0)
        + w.object_density * robust(a.object_density, b.object_density, 10.0)
        + w.circle_ratio * (a.circle_ratio - b.circle_ratio).abs()
        + w.slider_ratio * (a.slider_ratio - b.slider_ratio).abs()
}
fn matches_filters(base: BaseFeatures, set: u64, filters: &QueryFilters) -> bool {
    filters.min_ar.is_none_or(|x| base.ar >= x)
        && filters.max_ar.is_none_or(|x| base.ar <= x)
        && filters.min_bpm.is_none_or(|x| base.bpm >= x)
        && filters.max_bpm.is_none_or(|x| base.bpm <= x)
        && filters.beatmapset_id.is_none_or(|x| set == x)
}
fn write_index(root: &Path, name: &str, index: &IndexFile) -> Result<()> {
    let path = root.join("indexes").join(name);
    let temporary = path.with_extension("hnsw.tmp");
    let bytes = bincode::serialize(index)?;
    fs::write(&temporary, &bytes)?;
    fs::write(
        temporary.with_extension("sha256"),
        hex::encode(Sha256::digest(&bytes)),
    )?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(&temporary, &path)?;
    Ok(())
}
fn read_index(path: &Path) -> Result<IndexFile> {
    let bytes = fs::read(path)?;
    let checksum = path.with_extension("sha256");
    if checksum.exists() {
        let saved = fs::read_to_string(checksum)?;
        if saved.trim() != hex::encode(Sha256::digest(&bytes)) {
            anyhow::bail!("index checksum mismatch: {}", path.display());
        }
    }
    bincode::deserialize(&bytes).with_context(|| format!("invalid HNSW index: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BeatmapFeatureRecord, BeatmapMetadata, DifficultyVector, RawFeatureRecord};
    use tempfile::tempdir;

    fn add_record(
        store: &mut FeatureStore,
        id: u64,
        stars: f64,
        difficulty: [f32; 5],
    ) -> Result<BeatmapFeatureRecord> {
        let metadata = BeatmapMetadata {
            beatmap_id: id,
            beatmapset_id: id / 10,
            checksum: format!("checksum-{id}"),
            artist: "artist".into(),
            title: "title".into(),
            version: "version".into(),
            creator: "creator".into(),
            online_url: format!("https://osu.ppy.sh/b/{id}"),
            star_rating: stars,
        };
        let raw = RawFeatureRecord {
            beatmap_id: id,
            beatmapset_id: id / 10,
            analyzer_version: crate::ANALYZER_VERSION,
            mod_profile: 0,
            ..RawFeatureRecord::default()
        };
        store.append_raw(&metadata, &raw)?;
        Ok(BeatmapFeatureRecord {
            beatmap_id: id,
            beatmapset_id: id / 10,
            difficulty: DifficultyVector::from_array(difficulty),
            analyzer_version: crate::ANALYZER_VERSION,
            normalization_version: 1,
            mod_profile: 0,
            ..BeatmapFeatureRecord::default()
        })
    }

    fn index_with(records: &[BeatmapFeatureRecord], normalization_version: u32) -> IndexFile {
        let mut graph = Graph::new_params(WeightedL2, Params::new().ef_construction(200));
        let mut searcher = Searcher::new();
        let mut labels = Vec::new();
        for record in records {
            graph.insert(record.difficulty.as_array(), &mut searcher);
            labels.push(record.beatmap_id);
        }
        IndexFile {
            labels,
            graph,
            normalization_version,
        }
    }

    #[test]
    fn statistics_cover_records_split_between_main_and_delta() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        let main = add_record(&mut store, 10, 5.71, [0.1; 5])?;
        let delta = add_record(&mut store, 20, 5.79, [0.2; 5])?;
        store.write_normalized(1, &[main, delta])?;
        write_index(temp.path(), "difficulty-main.hnsw", &index_with(&[main], 1))?;
        write_index(
            temp.path(),
            "difficulty-delta.hnsw",
            &index_with(&[delta], 1),
        )?;

        validate_index_coverage(&store, 1)?;
        store.validate_star_section_stats(1)?;
        assert_eq!(
            store
                .star_section_stats(1)?
                .iter()
                .map(|stat| stat.sample_count)
                .sum::<u64>(),
            2
        );
        Ok(())
    }
}
