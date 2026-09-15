//! Recommendation similarity metric shared by the dataset exporter and the client.
//! It is not a mania_map_analyser difficulty rating.
use crate::{
    ManiaMmaRecord,
    config::{CORE_PATTERN_LIST, SV_AMOUNT_THRESHOLD},
};

#[derive(Debug, Clone, Copy)]
pub struct SimilarityDistance {
    pub skill: f64,
    pub pattern: f64,
    pub structure: f64,
    pub difficulty: f64,
    pub context: f64,
    pub total: f64,
}

fn distribution(a: &[f64], b: &[f64]) -> f64 {
    let sa: f64 = a.iter().sum();
    let sb: f64 = b.iter().sum();
    if sa == 0.0 && sb == 0.0 {
        return 0.0;
    }
    if sa == 0.0 || sb == 0.0 {
        return 1.0;
    }
    (a.iter()
        .zip(b)
        .map(|(a, b)| ((a / sa).sqrt() - (b / sb).sqrt()).powi(2))
        .sum::<f64>()
        / 2.0)
        .sqrt()
}

fn rms(a: &[f32], b: &[f32]) -> f64 {
    (a.iter()
        .zip(b)
        .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64)
        .sqrt()
}

pub fn is_sv(record: &ManiaMmaRecord) -> bool {
    record.sv_amount as f64 >= SV_AMOUNT_THRESHOLD
}

pub fn display_category(record: &ManiaMmaRecord) -> &str {
    if is_sv(record) {
        "SV"
    } else {
        &record.category
    }
}

pub fn distance(a: &ManiaMmaRecord, b: &ManiaMmaRecord) -> SimilarityDistance {
    let mut labels: Vec<&str> = a
        .subtypes
        .iter()
        .chain(&b.subtypes)
        .map(|(name, _)| name.as_str())
        .collect();
    labels.sort_unstable();
    labels.dedup();
    let values = |r: &ManiaMmaRecord| {
        labels
            .iter()
            .map(|name| {
                r.subtypes
                    .iter()
                    .find(|(n, _)| n == name)
                    .map_or(0.0, |(_, v)| *v as f64)
            })
            .collect::<Vec<_>>()
    };
    let subtype = distribution(&values(a), &values(b));
    let amount = |r: &ManiaMmaRecord| {
        CORE_PATTERN_LIST.map(|name| {
            r.bars
                .iter()
                .filter(|bar| bar.pattern == name)
                .map(|bar| bar.amount as f64)
                .sum::<f64>()
        })
    };
    let bpm = |r: &ManiaMmaRecord, name: &str| {
        let mut weight = 0.0;
        let mut sum = 0.0;
        for c in &r.clusters {
            if c.pattern == name && c.bpm > 0 {
                weight += c.amount as f64;
                sum += c.amount as f64 * (c.bpm as f64).ln();
            }
        }
        if weight > 0.0 { sum / weight } else { 0.0 }
    };
    let mut tempo = 0.0;
    let mut common = 0;
    for name in CORE_PATTERN_LIST {
        let x = bpm(a, name);
        let y = bpm(b, name);
        if x != 0.0 && y != 0.0 {
            tempo += ((x - y).abs() / std::f64::consts::LN_2).clamp(0.0, 1.0);
            common += 1;
        }
    }
    if common > 0 {
        tempo /= common as f64;
    }
    let pattern = 0.25 * distribution(&a.coverage.map(f64::from), &b.coverage.map(f64::from))
        + 0.35 * subtype
        + 0.2 * distribution(&amount(a), &amount(b))
        + 0.1 * f64::from(a.category != b.category)
        + 0.05 * tempo
        + 0.05 * f64::from(is_sv(a) != is_sv(b));
    let skill = (a.intensity[..3]
        .iter()
        .zip(&b.intensity[..3])
        .map(|(a, b)| ((1.0 + *a as f64) / (1.0 + *b as f64)).ln().abs())
        .sum::<f64>()
        / 3.0
        / std::f64::consts::LN_2)
        .clamp(0.0, 1.0);
    let structure = 0.7 * rms(&a.temporal, &b.temporal) + 0.3 * rms(&a.variation, &b.variation);
    let difficulty = (a.ln_note_ratio as f64 - b.ln_note_ratio as f64).abs();
    let context = (((1.0 + a.duration_seconds as f64) / (1.0 + b.duration_seconds as f64))
        .ln()
        .abs()
        / 4.0_f64.ln())
    .clamp(0.0, 1.0);
    let total =
        0.5 * pattern + 0.25 * skill + 0.15 * structure + 0.07 * difficulty + 0.03 * context;
    SimilarityDistance {
        skill,
        pattern,
        structure,
        difficulty,
        context,
        total,
    }
}
