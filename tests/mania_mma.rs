//! Regression tests for osu!mania key-pattern records.
//!
//! Synthetic charts live in `tests/fixtures/mma/`; expected values are the output of the pinned
//! mania_map_analyser revision for the same charts. They cover 4K/6K/7K and LN mode, which use different subtype tables and multipliers.

use std::path::PathBuf;

use osu_difficulty_lab::{
    ManiaAnalyzer, ManiaFeatureStore, ManiaGameMod, ManiaMmaAnalysis, analyze_mania_mma,
};
use serde_json::Value;

const FIXTURES: [&str; 4] = ["fixture-4k", "fixture-6k", "fixture-7k", "fixture-ln"];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mma")
        .join(name)
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixture(&format!("{name}.osu"))).expect("fixture beatmap")
}

fn expected(name: &str) -> Value {
    let raw = std::fs::read_to_string(fixture(&format!("{name}.expected.json")))
        .expect("expected report");
    serde_json::from_str(&raw).expect("expected report json")
}

fn assert_close(label: &str, expected: f64, actual: f64) {
    let tolerance = 1e-6_f64.max(expected.abs() * 1e-9);
    assert!(
        (expected - actual).abs() <= tolerance,
        "{label}: expected {expected} but got {actual}"
    );
}

fn assert_matches_reference(label: &str, analysis: &ManiaMmaAnalysis, expected: &Value) {
    assert_eq!(
        analysis.report.mode_tag,
        expected["ModeTag"].as_str().expect("mode tag"),
        "{label} ModeTag"
    );
    assert_eq!(
        analysis.report.category,
        expected["Category"].as_str().expect("category"),
        "{label} Category"
    );
    assert_close(
        &format!("{label} LNPercent"),
        expected["LNPercent"].as_f64().expect("ln percent"),
        analysis.report.ln_percent,
    );
    assert_close(
        &format!("{label} HBRowRatio"),
        expected["HBRowRatio"].as_f64().expect("hb ratio"),
        analysis.report.hb_row_ratio,
    );
    assert_close(
        &format!("{label} SVAmount"),
        expected["SVAmount"].as_f64().expect("sv amount"),
        analysis.report.sv_amount,
    );
    assert_close(
        &format!("{label} Duration"),
        expected["Duration"].as_f64().expect("duration"),
        analysis.report.duration,
    );

    let clusters = expected["Clusters"].as_array().expect("clusters");
    assert_eq!(
        clusters.len(),
        analysis.report.clusters.len(),
        "{label} cluster count"
    );
    for (index, cluster) in clusters.iter().enumerate() {
        let actual = &analysis.report.clusters[index];
        assert_eq!(
            actual.pattern,
            cluster["Pattern"].as_str().expect("pattern"),
            "{label} Clusters[{index}].Pattern"
        );
        assert_eq!(
            actual.mixed,
            cluster["Mixed"].as_bool().expect("mixed"),
            "{label} Clusters[{index}].Mixed"
        );
        assert_close(
            &format!("{label} Clusters[{index}].Amount"),
            cluster["Amount"].as_f64().expect("amount"),
            actual.amount,
        );
        assert_close(
            &format!("{label} Clusters[{index}].Importance"),
            cluster["Importance"].as_f64().expect("importance"),
            actual.importance,
        );
        assert_close(
            &format!("{label} Clusters[{index}].RatingMultiplier"),
            cluster["RatingMultiplier"].as_f64().expect("multiplier"),
            actual.rating_multiplier,
        );
        assert_close(
            &format!("{label} Clusters[{index}].BPM"),
            cluster["BPM"].as_f64().expect("bpm"),
            actual.bpm as f64,
        );

        let types = cluster["SpecificTypes"].as_array().expect("specific types");
        assert_eq!(
            types.len(),
            actual.specific_types.len(),
            "{label} Clusters[{index}].SpecificTypes length"
        );
        for (type_index, entry) in types.iter().enumerate() {
            assert_eq!(
                actual.specific_types[type_index].0,
                entry[0].as_str().expect("specific name"),
                "{label} Clusters[{index}].SpecificTypes[{type_index}]"
            );
            assert_close(
                &format!("{label} Clusters[{index}].SpecificTypes[{type_index}].ratio"),
                entry[1].as_f64().expect("specific ratio"),
                actual.specific_types[type_index].1,
            );
        }
    }
}

#[test]
fn key_patterns_match_the_pinned_reference_for_every_fixture_and_clock_rate() {
    for name in FIXTURES {
        let text = fixture_text(name);
        let expected = expected(name);
        for (key, game_mod) in [
            ("NM", ManiaGameMod::Nm),
            ("DT", ManiaGameMod::Dt),
            ("HT", ManiaGameMod::Ht),
        ] {
            let analysis = analyze_mania_mma(&text, game_mod).expect("analysis");
            assert_matches_reference(&format!("{name} {key}"), &analysis, &expected[key]);
        }
    }
}

#[test]
fn coverage_is_a_share_of_the_whole_chart() {
    // Coverage is the union of the intervals over the first-to-last note span.
    for name in FIXTURES {
        let text = fixture_text(name);
        let analysis = analyze_mania_mma(&text, ManiaGameMod::Nm).expect("analysis");
        let duration = analysis.report.duration;
        assert!(duration > 0.0);
        for (index, value) in analysis.features.coverage.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(value),
                "{name} coverage[{index}] out of range: {value}"
            );
            // Coverage x chart duration = covered seconds, which cannot exceed the chart.
            assert!(
                value * duration <= duration + 1e-6,
                "{name} coverage[{index}] exceeds chart duration"
            );
        }
        let total: f64 = analysis.features.coverage.iter().sum();
        assert!(total > 0.0, "{name} has no coverage at all");
    }
}

#[test]
fn ln_mode_charts_use_the_ln_multiplier_table() {
    let analysis = analyze_mania_mma(&fixture_text("fixture-ln"), ManiaGameMod::Nm).expect("NM");
    assert_eq!(analysis.report.mode_tag, "LN");
    assert!(analysis.report.ln_percent >= 0.9);
}

#[test]
fn clock_rates_are_analyzed_separately() {
    let text = fixture_text("fixture-4k");
    let no_mod = analyze_mania_mma(&text, ManiaGameMod::Nm).expect("NM");
    let double_time = analyze_mania_mma(&text, ManiaGameMod::Dt).expect("DT");
    let half_time = analyze_mania_mma(&text, ManiaGameMod::Ht).expect("HT");

    // The clock rate changes real density and BPM instead of scaling NoMod features.
    assert!(double_time.features.peak_nps > no_mod.features.peak_nps);
    assert!(half_time.features.peak_nps < no_mod.features.peak_nps);
    assert!((half_time.report.duration / no_mod.report.duration - 4.0 / 3.0).abs() < 0.01);
    assert!((no_mod.report.duration / double_time.report.duration - 1.5).abs() < 0.01);
}

#[test]
fn key_pattern_records_round_trip_through_the_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut store = ManiaFeatureStore::open(temp.path()).expect("store");

    let bytes = std::fs::read(fixture("fixture-4k.osu")).expect("fixture bytes");
    let (metadata, raw) = ManiaAnalyzer::new()
        .analyze_bytes(&bytes)
        .expect("raw analysis");
    assert!(store.append_raw(&metadata, &raw).expect("append raw"));

    let text = fixture_text("fixture-4k");
    for game_mod in ManiaGameMod::ALL {
        let record = analyze_mania_mma(&text, game_mod)
            .expect("analysis")
            .into_record(metadata.beatmap_id, metadata.beatmapset_id, game_mod);
        assert!(
            store
                .append_mma(&metadata.checksum, &record)
                .expect("append"),
            "first append stores the record"
        );
        assert!(
            !store
                .append_mma(&metadata.checksum, &record)
                .expect("append"),
            "unchanged checksum is skipped"
        );
    }

    assert_eq!(store.mma_record_count().expect("count"), 3);
    assert_eq!(
        store
            .mma_record_count_for(ManiaGameMod::Nm)
            .expect("count nm"),
        1
    );
    assert_eq!(store.mma_records().expect("records").len(), 3);

    let no_mod = store
        .mma_for_id(metadata.beatmap_id, ManiaGameMod::Nm)
        .expect("nm record");
    assert_eq!(no_mod.category, "Coordination");
    assert_eq!(no_mod.key_count, 4);
    assert_eq!(no_mod.game_mod, ManiaGameMod::Nm);
    assert_eq!(no_mod.mode_tag, "Mix");
    assert_eq!(no_mod.coverage.len(), 6);
    assert!(!no_mod.clusters.is_empty());

    let double_time = store
        .mma_for_id(metadata.beatmap_id, ManiaGameMod::Dt)
        .expect("dt record");
    assert_ne!(no_mod.duration_seconds, double_time.duration_seconds);
}
