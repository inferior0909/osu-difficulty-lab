//! Main-pattern categorisation.
//!
//! The main pattern is the specific type of the highest-importance cluster, not the
//! most-covered category. `Tech` marks an unstable rate inside that cluster (`Mixed` in mania_map_analyser).

use super::clustering::PatternCluster;
use super::config::{CATEGORY_JS_HS_SECONDARY_RATIO, IMPORTANT_CLUSTER_RATIO};

/// Always false, as in mania_map_analyser.
fn is_hybrid_chart() -> bool {
    false
}

pub fn categorise_chart(ordered_clusters: &[PatternCluster]) -> String {
    let Some(first) = ordered_clusters.first() else {
        return "Uncategorised".to_owned();
    };

    let mut important = Vec::new();
    for cluster in ordered_clusters {
        if (cluster.importance / first.importance) > IMPORTANT_CLUSTER_RATIO {
            important.push(cluster);
        } else {
            break;
        }
    }

    let cluster1 = important[0];
    let hybrid = is_hybrid_chart();
    let tech = cluster1.mixed;

    let name = match cluster1.specific_types.first() {
        Some((name, ratio)) if *ratio > 0.05 => name.clone(),
        _ if cluster1.specific_types.len() >= 2
            && cluster1.specific_types[0].0 == "Jumpstream"
            && cluster1.specific_types[1].0 == "Handstream" =>
        {
            let primary = cluster1.specific_types[0].1;
            let secondary = cluster1.specific_types[1].1;
            if primary > 0.0 && (secondary / primary) > CATEGORY_JS_HS_SECONDARY_RATIO {
                "Jumpstream/Handstream".to_owned()
            } else {
                cluster1.pattern.to_owned()
            }
        }
        _ => cluster1.pattern.to_owned(),
    };

    format!(
        "{name}{}{}",
        if hybrid { " Hybrid" } else { "" },
        if tech { " Tech" } else { "" }
    )
}
