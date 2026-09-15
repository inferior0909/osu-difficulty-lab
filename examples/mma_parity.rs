//! Parity harness for the mania key-pattern analysis.
//!
//! Compares this crate's analysis against a reference dump produced by the pinned
//! JavaScript implementation. The reference dump is generated outside the repository
//! (it lists absolute paths to real beatmaps), so this tool takes both files as arguments:
//!
//! ```text
//! cargo run --release --example mma_parity -- <corpus.json> <reference.json>
//! ```
//!
//! `corpus.json` is `{ "entries": [ { "path": "...", "keys": 4 }, ... ] }` and
//! `reference.json` is `{ "reports": [ { "path": ..., "mod": "NM", "report": {...}, ... } ] }`.

use std::collections::HashMap;
use std::fs;
use std::process::ExitCode;

use osu_difficulty_lab::{ManiaGameMod, analyze_mania_mma};
use serde_json::Value;

/// Comparison tolerance for values that travel through f64 from both sides.
const EPSILON: f64 = 1e-9;

#[derive(Default)]
struct Mismatches {
    total: usize,
    cases: usize,
    by_field: HashMap<String, usize>,
    details: Vec<String>,
}

impl Mismatches {
    fn record(&mut self, case: &str, field: &str, expected: &Value, actual: &str) {
        self.total += 1;
        let key = field
            .split_once('[')
            .map(|(head, _)| format!("{head}[]"))
            .unwrap_or_else(|| field.to_owned());
        *self.by_field.entry(key).or_default() += 1;
        if self.details.len() < 25 {
            self.details
                .push(format!("{case}: {field} expected {expected} got {actual}"));
        }
    }

    fn number(&mut self, case: &str, field: &str, expected: f64, actual: f64) {
        let tolerance = EPSILON.max(expected.abs() * 1e-12);
        if !expected.is_finite() || !actual.is_finite() || (expected - actual).abs() > tolerance {
            self.record(case, field, &Value::from(expected), &format!("{actual}"));
        }
    }
}

fn reference_mod(code: &str) -> ManiaGameMod {
    ManiaGameMod::from_code(code).expect("known mod in reference dump")
}

fn compare(
    case: &str,
    expected: &Value,
    actual: &osu_difficulty_lab::ManiaMmaAnalysis,
    out: &mut Mismatches,
) {
    let report = &expected["report"];

    out.number(
        case,
        "LNPercent",
        report["LNPercent"].as_f64().unwrap_or_default(),
        actual.report.ln_percent,
    );
    out.number(
        case,
        "HBRowRatio",
        report["HBRowRatio"].as_f64().unwrap_or_default(),
        actual.report.hb_row_ratio,
    );
    out.number(
        case,
        "SVAmount",
        report["SVAmount"].as_f64().unwrap_or_default(),
        actual.report.sv_amount,
    );
    out.number(
        case,
        "Duration",
        report["Duration"].as_f64().unwrap_or_default(),
        actual.report.duration,
    );

    let mode_tag = report["ModeTag"].as_str().unwrap_or_default();
    if mode_tag != actual.report.mode_tag {
        out.record(
            case,
            "ModeTag",
            &Value::from(mode_tag),
            actual.report.mode_tag,
        );
    }

    let category = report["Category"].as_str().unwrap_or_default();
    if category != actual.report.category {
        out.record(
            case,
            "Category",
            &Value::from(category),
            &actual.report.category,
        );
    }

    let expected_clusters = report["Clusters"].as_array().cloned().unwrap_or_default();
    if expected_clusters.len() != actual.report.clusters.len() {
        out.record(
            case,
            "Clusters.len",
            &Value::from(expected_clusters.len()),
            &actual.report.clusters.len().to_string(),
        );
        return;
    }

    for (index, expected_cluster) in expected_clusters.iter().enumerate() {
        let cluster = &actual.report.clusters[index];
        let field = |name: &str| format!("Clusters[{index}].{name}");

        let pattern = expected_cluster["Pattern"].as_str().unwrap_or_default();
        if pattern != cluster.pattern {
            out.record(
                case,
                &field("Pattern"),
                &Value::from(pattern),
                cluster.pattern,
            );
        }

        out.number(
            case,
            &field("Amount"),
            expected_cluster["Amount"].as_f64().unwrap_or_default(),
            cluster.amount,
        );
        out.number(
            case,
            &field("Importance"),
            expected_cluster["Importance"].as_f64().unwrap_or_default(),
            cluster.importance,
        );
        out.number(
            case,
            &field("RatingMultiplier"),
            expected_cluster["RatingMultiplier"]
                .as_f64()
                .unwrap_or_default(),
            cluster.rating_multiplier,
        );
        out.number(
            case,
            &field("BPM"),
            expected_cluster["BPM"].as_f64().unwrap_or_default(),
            cluster.bpm as f64,
        );

        let mixed = expected_cluster["Mixed"].as_bool().unwrap_or_default();
        if mixed != cluster.mixed {
            out.record(
                case,
                &field("Mixed"),
                &Value::from(mixed),
                &cluster.mixed.to_string(),
            );
        }

        let expected_types = expected_cluster["SpecificTypes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if expected_types.len() != cluster.specific_types.len() {
            out.record(
                case,
                &field("SpecificTypes.len"),
                &Value::from(expected_types.len()),
                &cluster.specific_types.len().to_string(),
            );
            continue;
        }
        for (type_index, expected_type) in expected_types.iter().enumerate() {
            let name = expected_type[0].as_str().unwrap_or_default();
            let ratio = expected_type[1].as_f64().unwrap_or_default();
            let (actual_name, actual_ratio) = &cluster.specific_types[type_index];
            if name != actual_name {
                out.record(
                    case,
                    &field(&format!("SpecificTypes[{type_index}].name")),
                    &Value::from(name),
                    actual_name,
                );
            }
            out.number(
                case,
                &field(&format!("SpecificTypes[{type_index}].ratio")),
                ratio,
                *actual_ratio,
            );
        }
    }

    let expected_core = expected["core"].as_array().cloned().unwrap_or_default();
    if expected_core.len() == 6 {
        for (index, value) in expected_core.iter().enumerate() {
            out.number(
                case,
                &format!("core[{index}]"),
                value.as_f64().unwrap_or_default(),
                actual.features.coverage[index],
            );
        }
    } else {
        out.record(
            case,
            "core.len",
            &Value::from(6),
            &expected_core.len().to_string(),
        );
    }

    let expected_subtypes = expected["subtypes"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if expected_subtypes.len() != actual.features.subtypes.len() {
        out.record(
            case,
            "subtypes.len",
            &Value::from(expected_subtypes.len()),
            &actual.features.subtypes.len().to_string(),
        );
    }
    for (name, ratio) in &actual.features.subtypes {
        match expected_subtypes.get(name) {
            Some(expected_ratio) => out.number(
                case,
                &format!("subtypes[{name}]"),
                expected_ratio.as_f64().unwrap_or_default(),
                *ratio,
            ),
            None => out.record(
                case,
                &format!("subtypes[{name}]"),
                &Value::from("absent"),
                &ratio.to_string(),
            ),
        }
    }

    let expected_intensity = expected["intensity"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if expected_intensity.len() == 4 {
        for (index, value) in expected_intensity.iter().enumerate() {
            out.number(
                case,
                &format!("intensity[{index}]"),
                value.as_f64().unwrap_or_default(),
                actual.features.intensity[index],
            );
        }
    } else {
        out.record(
            case,
            "intensity.len",
            &Value::from(4),
            &expected_intensity.len().to_string(),
        );
    }
    for (field, values) in [
        ("temporal", actual.features.temporal.as_slice()),
        ("variation", actual.features.variation.as_slice()),
    ] {
        let expected_values = expected[field].as_array().expect("reference feature array");
        assert_eq!(expected_values.len(), values.len());
        for (index, value) in values.iter().enumerate() {
            out.number(
                case,
                &format!("{field}[{index}]"),
                expected_values[index].as_f64().expect("numeric feature"),
                *value,
            );
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: mma_parity <corpus.json> <reference.json>");
        return ExitCode::from(2);
    }

    let corpus: Value =
        serde_json::from_str(&fs::read_to_string(&args[1]).expect("corpus")).expect("corpus json");
    let reference: Value = serde_json::from_str(&fs::read_to_string(&args[2]).expect("reference"))
        .expect("reference json");

    let mut sources: HashMap<String, String> = HashMap::new();
    for entry in corpus["entries"].as_array().cloned().unwrap_or_default() {
        let path = entry["path"].as_str().unwrap_or_default().to_owned();
        match fs::read_to_string(&path) {
            Ok(text) => {
                sources.insert(path, text);
            }
            Err(error) => panic!("cannot load {path}: {error}"),
        }
    }

    let mut out = Mismatches::default();
    let mut previous = None;
    for expected in reference["reports"].as_array().cloned().unwrap_or_default() {
        let path = expected["path"].as_str().unwrap_or_default();
        let text = sources.get(path).expect("every reference needs a source");
        let game_mod = reference_mod(expected["mod"].as_str().unwrap_or("NM"));
        let case = format!(
            "{} [{}]",
            path.rsplit('/').next().unwrap_or(path),
            game_mod.as_str()
        );

        match analyze_mania_mma(text, game_mod) {
            Ok(actual) => {
                out.cases += 1;
                compare(&case, &expected, &actual, &mut out);
                let record = actual.into_record(1, 1, game_mod);
                if let Some(previous) = &previous
                    && let Some(distance) = expected.get("distance_to_previous")
                {
                    let result = mania_pattern::similarity::distance(&record, previous);
                    for (name, actual) in [
                        ("total", result.total),
                        ("skill", result.skill),
                        ("pattern", result.pattern),
                        ("structure", result.structure),
                        ("difficulty", result.difficulty),
                        ("context", result.context),
                    ] {
                        let value = if name == "total" {
                            &distance[name]
                        } else {
                            &distance["components"][name]
                        };
                        let expected = value.as_f64().expect("numeric V4 distance");
                        // Persisted records intentionally use f32, unlike JS f64.
                        if !actual.is_finite() || (expected - actual).abs() > 2e-6 {
                            out.record(
                                &case,
                                &format!("distance.{name}"),
                                value,
                                &actual.to_string(),
                            );
                        }
                    }
                }
                previous = Some(record);
            }
            Err(error) => out.record(
                &case,
                "analyze",
                &Value::from("ok"),
                &format!("error: {error}"),
            ),
        }
    }

    println!("cases: {}", out.cases);
    println!("mismatches: {}", out.total);
    let mut fields: Vec<(String, usize)> = out.by_field.into_iter().collect();
    fields.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (field, count) in fields.iter().take(20) {
        println!("  {field}: {count}");
    }
    for detail in &out.details {
        println!("  {detail}");
    }

    if out.total == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
