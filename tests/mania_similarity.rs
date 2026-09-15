use osu_difficulty_lab::{ManiaGameMod, analyze_mania_mma_record};

#[test]
fn distance_matches_v4_for_all_fixture_pairs() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mma");
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("distances.expected.json")).unwrap())
            .unwrap();
    let mut records = Vec::new();
    for name in ["fixture-4k", "fixture-6k", "fixture-7k", "fixture-ln"] {
        let text = std::fs::read_to_string(root.join(format!("{name}.osu"))).unwrap();
        for game_mod in ManiaGameMod::ALL {
            records.push(analyze_mania_mma_record(&text, 1, 1, game_mod).unwrap());
        }
    }
    for (i, a) in records.iter().enumerate() {
        for (j, b) in records.iter().enumerate() {
            let distance = mania_pattern::similarity::distance(a, b);
            let reference = &expected[i][j];
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
    let mut sv = records[0].clone();
    sv.sv_amount = 2000.0;
    assert_eq!(mania_pattern::similarity::display_category(&sv), "SV");
    assert!((mania_pattern::similarity::distance(&sv, &records[0]).pattern - 0.05).abs() < 1e-9);
}
