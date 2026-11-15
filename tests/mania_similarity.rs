use osu_difficulty_lab::{ManiaGameMod, analyze_key_pattern_record};

#[test]
fn distance_matches_v4_for_all_fixture_pairs() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/key-pattern");
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("distances.expected.json")).unwrap())
            .unwrap();
    // The reference matrix holds NM, DT, HT of every chart in that order. Only the NM pairs still
    // apply: DT/HT records changed in 1.0, while the distance formula itself did not.
    let mut records = Vec::new();
    for name in ["fixture-4k", "fixture-6k", "fixture-7k", "fixture-ln"] {
        let text = std::fs::read_to_string(root.join(format!("{name}.osu"))).unwrap();
        let index = records.len() * ManiaGameMod::ALL.len();
        records.push((
            index,
            analyze_key_pattern_record(&text, 1, 1, ManiaGameMod::Nm).unwrap(),
        ));
    }
    for (i, a) in &records {
        for (j, b) in &records {
            let distance = mania_pattern::similarity::distance(a, b);
            let reference = &expected[*i][*j];
            for (name, value) in [
                ("total", distance.total),
                ("pattern", distance.pattern),
                ("skill", distance.skill),
                ("structure", distance.structure),
                ("difficulty", distance.difficulty),
                ("context", distance.context),
            ] {
                let expected = if name == "total" {
                    reference[name].as_f64().unwrap()
                } else {
                    reference["components"][name].as_f64().unwrap()
                };
                assert!(
                    (expected - value).abs() < 2e-6,
                    "pair {i},{j} {name}: {expected} != {value}"
                );
            }
        }
    }
    let no_mod = &records[0].1;
    let mut sv = no_mod.clone();
    sv.sv_amount = 2000.0;
    assert_eq!(mania_pattern::similarity::display_category(&sv), "SV");
    assert!((mania_pattern::similarity::distance(&sv, no_mod).pattern - 0.05).abs() < 1e-9);
}
