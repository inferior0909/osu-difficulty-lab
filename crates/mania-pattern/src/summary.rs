//! Pattern report summary.

use super::categorise::categorise_chart;
use super::chart::{Chart, NoteType};
use super::clustering::{PatternCluster, specific_clusters};
use super::config::{
    CORE_PATTERN_LIST, HB_ROW_RATIO_THRESHOLD, IMPORTANT_CLUSTER_RATIO, mode_tag_from_ln_ratio,
};
use super::patterns::{FoundPattern, find};
use super::primitives::{ln_percent, sv_time};

/// Long-note core patterns; RC charts are dropped.
const LN_CORE_PATTERNS: [&str; 3] = ["Coordination", "Density", "Wildcard"];

/// Share of rows carrying both a hold head and a short note.
fn hb_row_ratio(chart: &Chart) -> f64 {
    if chart.notes.is_empty() {
        return 0.0;
    }

    let mut hb_rows = 0usize;
    for row in &chart.notes {
        let has_head = row.data.contains(&NoteType::HoldHead);
        let has_normal = row.data.contains(&NoteType::Normal);
        if has_head && has_normal {
            hb_rows += 1;
        }
    }

    hb_rows as f64 / chart.notes.len() as f64
}

fn resolve_mode_tag(ln_ratio: f64, hb_ratio: f64) -> &'static str {
    let tag = mode_tag_from_ln_ratio(ln_ratio);
    if tag != "Mix" {
        return tag;
    }
    if hb_ratio >= HB_ROW_RATIO_THRESHOLD {
        return "HB";
    }
    "Mix"
}

#[derive(Debug, Clone)]
pub struct MmaReport {
    pub clusters: Vec<PatternCluster>,
    /// Filtered matches before clustering; the derived features reuse these intervals instead of matching again.
    pub patterns: Vec<FoundPattern>,
    pub category: String,
    pub ln_percent: f64,
    pub hb_row_ratio: f64,
    pub mode_tag: &'static str,
    pub sv_amount: f64,
    pub duration: f64,
}

impl MmaReport {
    pub fn important_clusters(&self) -> Vec<&PatternCluster> {
        let Some(first) = self.clusters.first() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for cluster in &self.clusters {
            if (cluster.importance / first.importance) > IMPORTANT_CLUSTER_RATIO {
                out.push(cluster);
            } else {
                break;
            }
        }
        out
    }
}

pub fn from_chart(chart: &Chart) -> MmaReport {
    let ln_ratio = ln_percent(chart);
    let hb_ratio = hb_row_ratio(chart);
    let mode_tag = resolve_mode_tag(ln_ratio, hb_ratio);

    let mut patterns = find(chart);
    if mode_tag == "RC" {
        patterns.retain(|pattern| !LN_CORE_PATTERNS.contains(&pattern.pattern));
    }

    let mut clusters: Vec<PatternCluster> = specific_clusters(&patterns, mode_tag)
        .into_iter()
        .filter(|cluster| cluster.bpm > 25 || cluster.bpm == 0)
        .collect();
    clusters.sort_by(|a, b| {
        b.amount
            .partial_cmp(&a.amount)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // A weaker cluster of the same pattern is dropped when a stronger and faster one exists.
    let mut keep = vec![true; clusters.len()];
    for (index, cluster) in clusters.iter().enumerate() {
        for (other_index, other) in clusters.iter().enumerate() {
            if index == other_index || other.pattern != cluster.pattern {
                continue;
            }
            if other.amount * 0.5 > cluster.amount && other.bpm > cluster.bpm {
                keep[index] = false;
                break;
            }
        }
    }

    let mut pruned = Vec::new();
    for pattern in CORE_PATTERN_LIST {
        let mut taken = 0usize;
        for (index, cluster) in clusters.iter().enumerate() {
            if !keep[index] || cluster.pattern != pattern {
                continue;
            }
            pruned.push(cluster.clone());
            taken += 1;
            if taken == 3 {
                break;
            }
        }
    }
    pruned.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let sv_amount = sv_time(chart);
    let category = categorise_chart(&pruned);

    MmaReport {
        clusters: pruned,
        patterns,
        category,
        ln_percent: ln_ratio,
        hb_row_ratio: hb_ratio,
        mode_tag,
        sv_amount,
        duration: chart.duration(),
    }
}
