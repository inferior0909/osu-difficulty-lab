//! Key-pattern records compared with osumania_map_analyser v3.0.0.
//!
//! `tests/fixtures/key-pattern/expected.json` holds the v3.0.0 reports for the synthetic charts
//! beside it: 4K/6K/7K plus an LN chart, which use different subtype tables and multipliers.
//! Its DT/HT entries are the NM report converted by the 1.0 rule: BPM x rate (rounded),
//! times / rate, importance recomputed.

use std::path::{Path, PathBuf};

use osu_difficulty_lab::{
    KeyPatternReport, ManiaAnalyzer, ManiaFeatureStore, ManiaGameMod, analyze_key_pattern,
    analyze_key_pattern_record,
};
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/key-pattern")
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(format!("{name}.osu"))).expect("fixture beatmap")
}

/// The report in the shape v3.0.0 prints it.
fn as_v3_json(report: &KeyPatternReport) -> Value {
    json!({
        "ModeTag": report.mode_tag,
        "Category": report.category,
        "LNPercent": report.ln_percent,
        "HBRowRatio": report.hb_row_ratio,
        "SVAmount": report.sv_amount,
        "Duration": report.duration,
        "Clusters": report.clusters.iter().map(|cluster| json!({
            "Pattern": cluster.pattern,
            "SpecificTypes": cluster.specific_types,
            "RatingMultiplier": cluster.rating_multiplier,
            "BPM": cluster.bpm,
            "Mixed": cluster.mixed,
            "Amount": cluster.amount,
            "Importance": cluster.importance,
        })).collect::<Vec<_>>(),
    })
}

/// Collects every field where `actual` differs from `expected`, allowing float noise in numbers.
fn differences(path: &str, actual: &Value, expected: &Value, out: &mut Vec<String>) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(e)) => {
            let (a, e) = (a.as_f64().expect("number"), e.as_f64().expect("number"));
            if (a - e).abs() > 1e-9_f64.max(e.abs() * 1e-12) {
                out.push(format!("{path}: expected {e} got {a}"));
            }
        }
        (Value::Array(a), Value::Array(e)) if a.len() == e.len() => {
            for (index, (a, e)) in a.iter().zip(e).enumerate() {
                differences(&format!("{path}[{index}]"), a, e, out);
            }
        }
        (Value::Object(a), Value::Object(e)) => {
            for (key, e) in e {
                differences(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    e,
                    out,
                );
            }
        }
        (a, e) if a == e => {}
        (a, e) => out.push(format!("{path}: expected {e} got {a}")),
    }
}

/// `reference` maps file names in `dir` to `{ "NM": report, "DT": report, "HT": report }`.
fn assert_matches_reference(dir: &Path, reference: &Path) {
    let reference: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(reference).expect("reference")).expect("json");
    let mut failures = Vec::new();
    for (name, reports) in &reference {
        let text = std::fs::read_to_string(dir.join(name)).expect("chart");
        for game_mod in ManiaGameMod::ALL {
            let case = format!("{name} {}", game_mod.as_str());
            match analyze_key_pattern(&text, game_mod) {
                Ok(analysis) => differences(
                    &case,
                    &as_v3_json(&analysis.report),
                    &reports[game_mod.as_str()],
                    &mut failures,
                ),
                Err(error) => failures.push(format!("{case}: {error}")),
            }
        }
    }
    let shown: Vec<&str> = failures.iter().take(25).map(String::as_str).collect();
    assert!(
        failures.is_empty(),
        "{} differences across {} charts, first ones:\n{}",
        failures.len(),
        reference.len(),
        shown.join("\n")
    );
}

#[test]
fn matches_v3_reference_for_fixtures() {
    assert_matches_reference(&fixtures(), &fixtures().join("expected.json"));
}

#[test]
fn dt_and_ht_measure_note_density_at_the_real_speed() {
    // Patterns come from the original speed (checked above); density must follow the clock rate.
    let text = fixture_text("fixture-4k");
    let nps = |game_mod| {
        analyze_key_pattern(&text, game_mod)
            .expect("analysis")
            .features
            .avg_nps
    };
    let no_mod = nps(ManiaGameMod::Nm);
    assert!((nps(ManiaGameMod::Dt) / no_mod - 1.5).abs() < 0.01);
    assert!((nps(ManiaGameMod::Ht) / no_mod - 0.75).abs() < 0.01);
}

#[test]
fn hit_objects_out_of_time_order_give_the_same_record() {
    let text = fixture_text("fixture-4k");
    let (head, objects) = text.split_once("[HitObjects]").expect("hit objects");
    let mut lines: Vec<&str> = objects
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines.reverse();
    let reversed = format!("{head}[HitObjects]\n{}\n", lines.join("\n"));
    let record =
        |text: &str| analyze_key_pattern_record(text, 1, 1, ManiaGameMod::Nm).expect("record");
    assert_eq!(record(&reversed), record(&text));
}

#[test]
fn negative_sv_amount_is_stored_as_zero() {
    let mut analysis =
        analyze_key_pattern(&fixture_text("fixture-4k"), ManiaGameMod::Nm).expect("analysis");
    analysis.report.sv_amount = -921_501.0;
    let record = analysis.into_record(1, 1, ManiaGameMod::Nm);
    assert_eq!(record.sv_amount, 0.0);
    mania_pattern::validate_record(&record).expect("valid record");
}

#[test]
fn coverage_is_a_share_of_the_whole_chart() {
    // Coverage is the union of the intervals over the first-to-last note span.
    for name in ["fixture-4k", "fixture-6k", "fixture-7k", "fixture-ln"] {
        let analysis =
            analyze_key_pattern(&fixture_text(name), ManiaGameMod::Nm).expect("analysis");
        for (index, value) in analysis.features.coverage.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(value),
                "{name} coverage[{index}] out of range: {value}"
            );
        }
        let total: f64 = analysis.features.coverage.iter().sum();
        assert!(total > 0.0, "{name} has no coverage at all");
    }
}

#[test]
fn ln_mode_charts_use_the_ln_multiplier_table() {
    let analysis = analyze_key_pattern(&fixture_text("fixture-ln"), ManiaGameMod::Nm).expect("NM");
    assert_eq!(analysis.report.mode_tag, "LN");
    assert!(analysis.report.ln_percent >= 0.9);
}

#[test]
fn key_pattern_records_round_trip_through_the_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut store = ManiaFeatureStore::open(temp.path()).expect("store");
    assert!(!store.prepare_key_pattern_analysis().expect("prepare"));

    let bytes = std::fs::read(fixtures().join("fixture-4k.osu")).expect("fixture bytes");
    let (metadata, raw) = ManiaAnalyzer::new()
        .analyze_bytes(&bytes)
        .expect("raw analysis");
    assert!(store.append_raw(&metadata, &raw).expect("append raw"));

    let text = fixture_text("fixture-4k");
    for game_mod in ManiaGameMod::ALL {
        let record = analyze_key_pattern(&text, game_mod)
            .expect("analysis")
            .into_record(metadata.beatmap_id, metadata.beatmapset_id, game_mod);
        assert!(
            store
                .append_key_pattern(&metadata.checksum, &record)
                .expect("append")
        );
        assert!(
            !store
                .append_key_pattern(&metadata.checksum, &record)
                .expect("append"),
            "unchanged checksum is skipped"
        );
    }

    assert_eq!(store.key_pattern_count().expect("count"), 3);
    assert_eq!(
        store
            .key_pattern_count_for(ManiaGameMod::Nm)
            .expect("count"),
        1
    );
    assert_eq!(store.key_pattern_records().expect("records").len(), 3);

    let no_mod = store
        .key_pattern_for_id(metadata.beatmap_id, ManiaGameMod::Nm)
        .expect("nm record");
    assert_eq!(no_mod.category, "Coordination");
    assert_eq!(no_mod.key_count, 4);
    assert_eq!(no_mod.mode_tag, "Mix");
    let double_time = store
        .key_pattern_for_id(metadata.beatmap_id, ManiaGameMod::Dt)
        .expect("dt record");
    assert_eq!(double_time.category, no_mod.category);
    assert!((double_time.duration_seconds * 1.5 - no_mod.duration_seconds).abs() < 1e-3);

    // A rule change retires every stored record so the next run analyses them again.
    assert!(!store.prepare_key_pattern_analysis().expect("same rules"));
    rusqlite::Connection::open(temp.path().join("mania-metadata.sqlite"))
        .expect("database")
        .execute(
            "UPDATE mania_state SET value='older-rules' WHERE key='key_pattern_ruleset'",
            [],
        )
        .expect("old rules");
    assert!(store.prepare_key_pattern_analysis().expect("new rules"));
    assert_eq!(store.key_pattern_count().expect("count"), 0);
    assert!(
        !store
            .current_key_pattern_matches(metadata.beatmap_id, &metadata.checksum, ManiaGameMod::Nm)
            .expect("lookup")
    );
}
