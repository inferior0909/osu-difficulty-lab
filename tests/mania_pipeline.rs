use std::fs;

use anyhow::Result;
use osu_difficulty_lab::{
    ManiaAnalyzeError, ManiaAnalyzer, ManiaFeatureStore, ManiaNormalizer, ManiaSimilarityQuery,
    ManiaSimilarityStore, build_mania_index, export_mania_csv, export_mania_parquet,
    fit_mania_normalizer, validate_mania_index_coverage,
};
use tempfile::tempdir;

fn mania_map(id: u64, set: u64, keys: u8, step: usize, repeated: bool) -> Vec<u8> {
    let objects = (0..48)
        .map(|index| {
            let lane = if repeated { 0 } else { index % keys as usize };
            let x = ((lane as f64 + 0.5) * 512.0 / keys as f64).floor() as usize;
            format!("{x},192,{},1,0,0:0:0:0:", index * step)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "osu file format v14\n\n[General]\nMode:3\n\n[Metadata]\nTitle:Map {id}\nArtist:Artist\nCreator:Mapper\nVersion:{keys}K\nBeatmapID:{id}\nBeatmapSetID:{set}\n\n[Difficulty]\nCircleSize:{keys}\nOverallDifficulty:8\n\n[TimingPoints]\n0,500,4,2,0,100,1,0\n\n[HitObjects]\n{objects}\n"
    )
    .into_bytes()
}

#[test]
fn mania_pipeline_analyzes_normalizes_indexes_queries_and_exports() -> Result<()> {
    let temp = tempdir()?;
    let mut store = ManiaFeatureStore::open(temp.path())?;
    let analyzer = ManiaAnalyzer::new();
    let corpus = [
        mania_map(10, 1, 4, 140, false),
        mania_map(20, 2, 4, 145, false),
        mania_map(30, 3, 4, 100, true),
        mania_map(60, 6, 6, 140, false),
        mania_map(70, 7, 7, 140, false),
    ];
    for bytes in &corpus {
        let (metadata, record) = analyzer.analyze_bytes(bytes)?;
        assert!(store.append_raw(&metadata, &record)?);
    }
    assert!(matches!(
        analyzer.analyze_bytes(&mania_map(50, 5, 5, 140, false)),
        Err(ManiaAnalyzeError::UnsupportedKeyCount(5))
    ));
    store.set_scan_counts(5, 1, 0)?;
    fit_mania_normalizer(&mut store, 1)?;
    build_mania_index(&store, 1)?;
    validate_mania_index_coverage(&store, 1)?;

    let similarity = ManiaSimilarityStore::open(temp.path(), 1)?;
    let query = ManiaSimilarityQuery {
        result_limit: 2,
        include_same_set: false,
    };
    let by_id = similarity.query_by_id(&store, 10, query)?;
    assert_eq!(by_id.first().map(|result| result.beatmap_id), Some(20));
    assert!(by_id.iter().all(|result| result.key_count == 4));

    let (_, external_raw) = analyzer.analyze_bytes(&corpus[0])?;
    let external = ManiaNormalizer::load(temp.path(), 1)?.transform(&external_raw)?;
    let by_file = similarity.query_record(&store, external, query)?;
    assert_eq!(
        by_id
            .iter()
            .map(|result| result.beatmap_id)
            .collect::<Vec<_>>(),
        by_file
            .iter()
            .map(|result| result.beatmap_id)
            .collect::<Vec<_>>()
    );

    let records = store.normalized_records(1)?;
    let csv = temp.path().join("mania.csv");
    let parquet = temp.path().join("mania.parquet");
    export_mania_csv(&csv, &records)?;
    export_mania_parquet(&parquet, &records)?;
    assert!(fs::metadata(csv)?.len() > 0);
    assert!(fs::metadata(parquet)?.len() > 0);
    assert_eq!(store.scan_counts()?, (5, 1, 0));
    fs::create_dir_all(temp.path().join("beatmaps"))?;
    for bytes in &corpus {
        let (metadata, _) = analyzer.analyze_bytes(bytes)?;
        fs::write(
            temp.path()
                .join("beatmaps")
                .join(format!("{}.osu", metadata.beatmap_id)),
            bytes,
        )?;
    }
    assert_eq!(
        osu_difficulty_lab::export_mania_mod_features(temp.path(), 1)?,
        10
    );
    let sidecar = temp.path().join("mania-mod-features-v1.bin");
    let before = fs::read(&sidecar)?;
    assert_eq!(&before[..8], b"ODLMMV1\0");
    let mut cursor = std::io::Cursor::new(&before[8..]);
    let normalizer = ManiaNormalizer::load(temp.path(), 1)?;
    for _ in 0..10 {
        let record: osu_difficulty_lab::ManiaModFeatureRecord =
            bincode::deserialize_from(&mut cursor)?;
        assert_ne!(record.game_mod, osu_difficulty_lab::ManiaGameMod::Nm);
        let source = fs::read(
            temp.path()
                .join("beatmaps")
                .join(format!("{}.osu", record.beatmap_id)),
        )?;
        let (_, raw) = analyzer.analyze_bytes_with_mod(&source, record.game_mod)?;
        assert_eq!(record.record, normalizer.transform(&raw)?);
    }
    fs::write(temp.path().join("beatmaps/10.osu"), b"changed source")?;
    assert!(osu_difficulty_lab::export_mania_mod_features(temp.path(), 1).is_err());
    assert_eq!(
        fs::read(sidecar)?,
        before,
        "failed export must retain the previous file"
    );
    Ok(())
}
