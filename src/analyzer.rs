use std::cmp::Ordering;

use anyhow::{Result, bail};
use rosu_pp::any::DifficultyAttributes;
use rosu_pp::{Beatmap, Difficulty};
use sha2::Digest;

use crate::{
    ANALYZER_VERSION, AnalyzerConfig, BaseFeatures, BeatmapMetadata, DifficultyVector,
    OverlapStatistics, RawFeatureRecord,
};

#[derive(Debug, Clone)]
pub struct ParsedBeatmap {
    pub metadata: BeatmapMetadata,
    pub ar: f64,
    pub od: f64,
    pub cs: f64,
    pub hp: f64,
    pub bpm: f64,
    objects: Vec<HitObject>,
}

#[derive(Debug, Clone)]
struct HitObject {
    x: f64,
    y: f64,
    time: f64,
    end_time: f64,
    kind: ObjectKind,
    path: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectKind {
    Circle,
    Slider,
    Spinner,
}

#[derive(Debug, Clone)]
pub struct Analyzer {
    config: AnalyzerConfig,
}

impl Analyzer {
    pub fn new(config: AnalyzerConfig) -> Self {
        Self { config }
    }
    pub fn config(&self) -> &AnalyzerConfig {
        &self.config
    }

    pub fn parse(&self, bytes: &[u8]) -> Result<ParsedBeatmap> {
        parse_beatmap(bytes)
    }

    pub fn analyze_bytes(&self, bytes: &[u8]) -> Result<(BeatmapMetadata, RawFeatureRecord)> {
        let parsed = self.parse(bytes)?;
        let map = Beatmap::from_bytes(bytes)?;
        let attrs = match Difficulty::new().calculate(&map) {
            DifficultyAttributes::Osu(value) => value,
            _ => bail!("only osu!standard beatmaps are supported"),
        };
        let overlap = self.overlap(&parsed.objects, parsed.ar, parsed.cs);
        let duration_ms = parsed
            .objects
            .last()
            .map(|object| object.end_time)
            .unwrap_or(0.0)
            - parsed
                .objects
                .first()
                .map(|object| object.time)
                .unwrap_or(0.0);
        let count = parsed.objects.len() as f32;
        let circles = parsed
            .objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Circle)
            .count() as f32;
        let sliders = parsed
            .objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Slider)
            .count() as f32;
        let spinners = parsed
            .objects
            .iter()
            .filter(|object| object.kind == ObjectKind::Spinner)
            .count() as f32;
        let base = BaseFeatures {
            bpm: parsed.bpm as f32,
            ar: parsed.ar as f32,
            od: parsed.od as f32,
            cs: parsed.cs as f32,
            hp: parsed.hp as f32,
            length_seconds: (duration_ms / 1000.0).max(0.0) as f32,
            object_count: count,
            object_density: if duration_ms > 0.0 {
                (count as f64 / (duration_ms / 1000.0)) as f32
            } else {
                0.0
            },
            circle_ratio: ratio(circles, count),
            slider_ratio: ratio(sliders, count),
            spinner_ratio: ratio(spinners, count),
            max_combo: attrs.max_combo as f32,
        };
        let raw_difficulty = DifficultyVector {
            aim: attrs.aim as f32,
            speed: attrs.speed as f32,
            reading: reading_baseline(&parsed.objects, parsed.ar, &self.config) as f32,
            flashlight: attrs.flashlight as f32,
            overlap: overlap.peak,
        };
        let record = RawFeatureRecord {
            beatmap_id: parsed.metadata.beatmap_id,
            beatmapset_id: parsed.metadata.beatmapset_id,
            raw_difficulty,
            base,
            overlap,
            analyzer_version: ANALYZER_VERSION,
            mod_profile: 0,
        };
        Ok((parsed.metadata, record))
    }

    fn overlap(&self, objects: &[HitObject], ar: f64, cs: f64) -> OverlapStatistics {
        let cfg = &self.config.overlap;
        let radius = 54.4 - 4.48 * cs;
        let preempt = ar_to_preempt(ar);
        let mut per_object = vec![Vec::<PairDetail>::new(); objects.len()];
        let mut stack = 0_u32;
        let mut slider = 0_u32;
        let mut crossing = 0_u32;
        let mut compared = 0_u32;
        for current in 0..objects.len() {
            let object = &objects[current];
            if object.kind == ObjectKind::Spinner {
                continue;
            }
            let earliest = object.time - cfg.overlap_window_ms;
            for prior in (0..current).rev() {
                let other = &objects[prior];
                if other.time < earliest {
                    break;
                }
                if other.kind == ObjectKind::Spinner {
                    continue;
                }
                let visible_overlap = interval_overlap(
                    object.time - preempt,
                    object.end_time,
                    other.time - preempt,
                    other.end_time,
                );
                if visible_overlap <= 0.0 {
                    continue;
                }
                compared += 1;
                let dt = (object.time - other.time).abs();
                let time_weight = (-dt / cfg.temporal_tau_ms).exp();
                let visibility = (visible_overlap / preempt.min(preempt).max(1.0)) * time_weight;
                let distance = min_object_distance(object, other);
                let q = distance / (2.0 * radius).max(1.0);
                if q > cfg.maximum_distance_ratio {
                    continue;
                }
                let spatial = 0.8 * (1.0 - q).max(0.0).powi(2)
                    + 0.2 * (1.0 - q / cfg.maximum_distance_ratio).max(0.0).powi(2);
                let circle_overlap =
                    if object.kind != ObjectKind::Slider || other.kind != ObjectKind::Slider {
                        spatial
                    } else {
                        0.0
                    };
                let slider_occlusion =
                    if object.kind == ObjectKind::Slider || other.kind == ObjectKind::Slider {
                        spatial
                    } else {
                        0.0
                    };
                let stack_pressure = if q <= cfg.stack_distance_ratio {
                    spatial * speed_pressure(dt)
                } else {
                    0.0
                };
                let ambiguity = spatial / ((current - prior).max(1) as f64).sqrt();
                let path_crossing =
                    movement_crossing(objects, prior, current, radius) * time_weight;
                let score = visibility
                    * speed_pressure(dt)
                    * (cfg.circle_weight * circle_overlap
                        + cfg.slider_weight * slider_occlusion
                        + cfg.ambiguity_weight * ambiguity
                        + cfg.stack_weight * stack_pressure
                        + cfg.crossing_weight * path_crossing);
                if stack_pressure > 0.0 {
                    stack += 1;
                }
                if slider_occlusion > 0.0 {
                    slider += 1;
                }
                if path_crossing > 0.0 {
                    crossing += 1;
                }
                if score > 0.0 {
                    per_object[current].push(PairDetail { score });
                }
            }
        }
        let mut strain = 0.0_f64;
        let mut peaks = Vec::new();
        let mut section_end = 0.0_f64;
        let mut previous_time = None;
        let mut sustained = 0_u32;
        for (index, object) in objects.iter().enumerate() {
            if object.kind == ObjectKind::Spinner {
                continue;
            }
            while object.time > section_end {
                peaks.push(strain);
                section_end += cfg.section_length_ms;
            }
            let pairs = &per_object[index];
            let max_score = pairs.iter().map(|pair| pair.score).fold(0.0_f64, f64::max);
            let sum_score: f64 = pairs.iter().map(|pair| pair.score).sum();
            let object_score = 0.65 * max_score + 0.35 * sum_score.ln_1p();
            let delta = previous_time
                .map(|previous| object.time - previous)
                .unwrap_or(0.0_f64)
                .max(0.0_f64);
            strain = strain * cfg.strain_decay_base.powf(delta / 1000.0) + object_score;
            if object_score > 0.0 {
                sustained += 1;
            }
            previous_time = Some(object.time);
        }
        peaks.push(strain);
        peaks.sort_by(|left, right| right.partial_cmp(left).unwrap_or(Ordering::Equal));
        let weighted_peak: f64 = peaks
            .iter()
            .enumerate()
            .map(|(i, value)| value * cfg.peak_weight_decay.powi(i as i32))
            .sum();
        let p95 = if peaks.is_empty() {
            0.0
        } else {
            peaks[((peaks.len() - 1) as f64 * 0.05).floor() as usize]
        };
        OverlapStatistics {
            peak: weighted_peak as f32,
            p95: p95 as f32,
            sustained_ratio: sustained as f32 / objects.len().max(1) as f32,
            stack_rate: stack as f32 / compared.max(1) as f32,
            slider_occlusion_rate: slider as f32 / compared.max(1) as f32,
            path_crossing_rate: crossing as f32 / compared.max(1) as f32,
        }
    }
}

#[derive(Clone, Copy)]
struct PairDetail {
    score: f64,
}

fn ratio(numerator: f32, denominator: f32) -> f32 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    }
}
fn interval_overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}
fn speed_pressure(delta: f64) -> f64 {
    (200.0 / delta.max(50.0)).sqrt().clamp(0.5, 2.0)
}
fn ar_to_preempt(ar: f64) -> f64 {
    if ar < 5.0 {
        1800.0 - 120.0 * ar
    } else {
        1200.0 - 150.0 * (ar - 5.0)
    }
}

fn reading_baseline(objects: &[HitObject], ar: f64, config: &AnalyzerConfig) -> f64 {
    let mut peaks = Vec::new();
    let mut section_end = config.reading_section_ms;
    let mut count = 0_u32;
    for object in objects
        .iter()
        .filter(|object| object.kind != ObjectKind::Spinner)
    {
        while object.time > section_end {
            peaks.push(count as f64 / (config.reading_section_ms / 1000.0));
            count = 0;
            section_end += config.reading_section_ms;
        }
        count += 1;
    }
    peaks.push(count as f64 / (config.reading_section_ms / 1000.0));
    peaks.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
    let ar_pressure = 1.0 + (ar.clamp(0.0, 11.0) / 10.0);
    peaks
        .iter()
        .enumerate()
        .map(|(i, peak)| peak * ar_pressure * config.reading_peak_weight_decay.powi(i as i32))
        .sum()
}

fn min_object_distance(a: &HitObject, b: &HitObject) -> f64 {
    match (a.kind == ObjectKind::Slider, b.kind == ObjectKind::Slider) {
        (false, false) => distance((a.x, a.y), (b.x, b.y)),
        (true, false) => point_polyline_distance((b.x, b.y), &a.path),
        (false, true) => point_polyline_distance((a.x, a.y), &b.path),
        (true, true) => polyline_distance(&a.path, &b.path),
    }
}
fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}
fn point_polyline_distance(point: (f64, f64), path: &[(f64, f64)]) -> f64 {
    path.windows(2)
        .map(|segment| point_segment_distance(point, segment[0], segment[1]))
        .fold(f64::INFINITY, f64::min)
        .min(
            path.first()
                .map(|value| distance(point, *value))
                .unwrap_or(f64::INFINITY),
        )
}
fn polyline_distance(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    a.windows(2)
        .flat_map(|left| {
            b.windows(2)
                .map(move |right| segment_distance(left[0], left[1], right[0], right[1]))
        })
        .fold(f64::INFINITY, f64::min)
}
fn point_segment_distance(point: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let t = if dx == 0.0 && dy == 0.0 {
        0.0
    } else {
        (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0)
    };
    distance(point, (a.0 + t * dx, a.1 + t * dy))
}
fn segment_distance(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> f64 {
    if segments_intersect(a, b, c, d) {
        0.0
    } else {
        point_segment_distance(a, c, d)
            .min(point_segment_distance(b, c, d))
            .min(point_segment_distance(c, a, b))
            .min(point_segment_distance(d, a, b))
    }
}
fn cross(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}
fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let ab1 = cross(a, b, c);
    let ab2 = cross(a, b, d);
    let cd1 = cross(c, d, a);
    let cd2 = cross(c, d, b);
    ab1 * ab2 <= 0.0 && cd1 * cd2 <= 0.0
}
fn movement_crossing(objects: &[HitObject], prior: usize, current: usize, radius: f64) -> f64 {
    if prior == 0 || current == 0 {
        return 0.0;
    }
    let a = &objects[prior - 1];
    let b = &objects[prior];
    let c = &objects[current - 1];
    let d = &objects[current];
    if segment_distance((a.x, a.y), (b.x, b.y), (c.x, c.y), (d.x, d.y)) > radius {
        return 0.0;
    }
    let u = (b.x - a.x, b.y - a.y);
    let v = (d.x - c.x, d.y - c.y);
    let denom = ((u.0 * u.0 + u.1 * u.1).sqrt() * (v.0 * v.0 + v.1 * v.1).sqrt()).max(1.0);
    ((u.0 * v.1 - u.1 * v.0).abs() / denom).clamp(0.0, 1.0)
}

fn parse_beatmap(bytes: &[u8]) -> Result<ParsedBeatmap> {
    let text = std::str::from_utf8(bytes)?;
    let mut section = "";
    let mut mode = 0_i32;
    let mut beatmap_id = 0_u64;
    let mut beatmapset_id = 0_u64;
    let mut artist = String::new();
    let mut title = String::new();
    let mut version = String::new();
    let mut creator = String::new();
    let mut ar = 5.0;
    let mut od = 5.0;
    let mut cs = 5.0;
    let mut hp = 5.0;
    let mut bpm = 0.0;
    let mut objects = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
    {
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }
        match section {
            "General" => {
                if let Some((key, value)) = line.split_once(':')
                    && key.trim() == "Mode"
                {
                    mode = value.trim().parse().unwrap_or(-1);
                }
            }
            "Metadata" => {
                if let Some((key, value)) = line.split_once(':') {
                    match key.trim() {
                        "BeatmapID" => beatmap_id = value.trim().parse().unwrap_or(0),
                        "BeatmapSetID" => beatmapset_id = value.trim().parse().unwrap_or(0),
                        "Artist" => artist = value.trim().into(),
                        "Title" => title = value.trim().into(),
                        "Version" => version = value.trim().into(),
                        "Creator" => creator = value.trim().into(),
                        _ => {}
                    }
                }
            }
            "Difficulty" => {
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim().parse::<f64>().unwrap_or(5.0);
                    match key.trim() {
                        "ApproachRate" => ar = value,
                        "OverallDifficulty" => od = value,
                        "CircleSize" => cs = value,
                        "HPDrainRate" => hp = value,
                        _ => {}
                    }
                }
            }
            "TimingPoints" => {
                if bpm == 0.0 {
                    let values: Vec<_> = line.split(',').collect();
                    if values.len() > 1 {
                        let beat = values[1].parse::<f64>().unwrap_or(0.0);
                        if beat > 0.0 {
                            bpm = 60000.0 / beat;
                        }
                    }
                }
            }
            "HitObjects" => {
                let values: Vec<_> = line.split(',').collect();
                if values.len() >= 5 {
                    let x = values[0].parse().unwrap_or(0.0);
                    let y = values[1].parse().unwrap_or(0.0);
                    let time = values[2].parse().unwrap_or(0.0);
                    let flags = values[3].parse::<u32>().unwrap_or(0);
                    let kind = if flags & 8 != 0 {
                        ObjectKind::Spinner
                    } else if flags & 2 != 0 {
                        ObjectKind::Slider
                    } else {
                        ObjectKind::Circle
                    };
                    let mut path = vec![(x, y)];
                    if kind == ObjectKind::Slider && values.len() > 5 {
                        for part in values[5].split('|').skip(1) {
                            if let Some((px, py)) = part.split_once(':') {
                                path.push((px.parse().unwrap_or(x), py.parse().unwrap_or(y)));
                            }
                        }
                    }
                    let end_time = if kind == ObjectKind::Spinner {
                        values
                            .get(5)
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(time)
                    } else {
                        time
                    };
                    objects.push(HitObject {
                        x,
                        y,
                        time,
                        end_time,
                        kind,
                        path,
                    });
                }
            }
            _ => {}
        }
    }
    if mode != 0 {
        bail!("only osu!standard beatmaps are supported");
    }
    objects.sort_by(|left, right| {
        left.time
            .partial_cmp(&right.time)
            .unwrap_or(Ordering::Equal)
    });
    let digest = sha2::Sha256::digest(bytes);
    // Very early official packs predate BeatmapID/BeatmapSetID metadata. Keep them
    // queryable with a deterministic local ID while retaining the full checksum.
    if beatmap_id == 0 {
        beatmap_id = u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 prefix"));
        if beatmap_id == 0 {
            beatmap_id = 1;
        }
    }
    Ok(ParsedBeatmap {
        metadata: BeatmapMetadata {
            beatmap_id,
            beatmapset_id,
            checksum: hex::encode(digest),
            artist,
            title,
            version,
            creator,
            online_url: format!("https://osu.ppy.sh/b/{beatmap_id}"),
        },
        ar,
        od,
        cs,
        hp,
        bpm,
        objects,
    })
}
