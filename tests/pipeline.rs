use anyhow::Result;
use osu_difficulty_lab::{
    Analyzer, AnalyzerConfig, FeatureStore, SimilarityQuery, SimilarityStore, build_main_index,
    fit_normalizer,
};
use tempfile::tempdir;

fn map(id: u64, positions: &[(i32, i32, i32)]) -> Vec<u8> {
    let objects = positions
        .iter()
        .map(|(x, y, time)| format!("{x},{y},{time},1,0,0:0:0:0:"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("osu file format v14\n\n[General]\nMode: 0\n\n[Metadata]\nTitle: Test\nArtist: Test\nCreator: Test\nVersion: Test\nBeatmapID: {id}\nBeatmapSetID: {}\n\n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:7\nApproachRate:9\n\n[TimingPoints]\n0,500,4,2,0,100,1,0\n\n[HitObjects]\n{objects}\n",id/10).into_bytes()
}

#[test]
fn pipeline_normalizes_builds_and_queries() -> Result<()> {
    let temp = tempdir()?;
    let mut store = FeatureStore::open(temp.path())?;
    let analyzer = Analyzer::new(AnalyzerConfig::default());
    for (id, positions) in [
        (10, vec![(64, 64, 0), (448, 320, 500), (64, 64, 1000)]),
        (20, vec![(64, 64, 0), (448, 320, 500), (64, 64, 1000)]),
        (30, vec![(256, 192, 0), (257, 192, 80), (256, 192, 160)]),
    ] {
        let (metadata, record) = analyzer.analyze_bytes(&map(id, &positions))?;
        assert!(store.append_raw(&metadata, &record)?);
    }
    let normalizer = fit_normalizer(&mut store, 1)?;
    assert_eq!(normalizer.version, 1);
    assert_eq!(store.normalized_records(1)?.len(), 3);
    build_main_index(&store, 1)?;
    let query = SimilarityQuery {
        beatmap_id: 10,
        result_limit: 2,
        ..SimilarityQuery::default()
    };
    let results = SimilarityStore::open(temp.path(), 1)?.query(&store, query)?;
    assert_eq!(results.first().map(|result| result.beatmap_id), Some(20));
    Ok(())
}

#[test]
fn true_overlap_is_higher_than_spread_density() -> Result<()> {
    let analyzer = Analyzer::new(AnalyzerConfig::default());
    let (_, spread) =
        analyzer.analyze_bytes(&map(100, &[(64, 64, 0), (448, 320, 80), (64, 64, 160)]))?;
    let (_, stack) =
        analyzer.analyze_bytes(&map(110, &[(256, 192, 0), (256, 192, 80), (256, 192, 160)]))?;
    assert!(stack.raw_difficulty.overlap > spread.raw_difficulty.overlap);
    Ok(())
}
