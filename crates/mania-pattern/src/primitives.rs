//! Per-row primitive calculation.
//!
//! Primitives are the only input to pattern matching: each row records its column count, repeated keys (jacks), direction and hold state.

use super::chart::{Chart, NoteType};
use super::config::{SV_EXTREME_BPM_MAX, SV_EXTREME_BPM_MIN, SV_EXTREME_BPM_RATIO, SV_SPEED_EPS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    None,
    Left,
    Right,
    Outwards,
    Inwards,
}

#[derive(Debug, Clone)]
pub struct Primitive {
    pub index: usize,
    pub time: f64,
    pub ms_per_beat: f64,
    pub beat_length: f64,
    pub notes: usize,
    pub jacks: usize,
    pub direction: Direction,
    pub roll: bool,
    pub keys: usize,
    pub left_hand_keys: usize,
    pub ln_heads: Vec<usize>,
    pub ln_bodies: Vec<usize>,
    pub ln_tails: Vec<usize>,
    pub normal_notes: Vec<usize>,
    pub raw_notes: Vec<usize>,
}

pub fn keys_on_left_hand(keymode: usize) -> usize {
    match keymode {
        3 | 4 => 2,
        5 | 6 => 3,
        7 | 8 => 4,
        9 | 10 => 5,
        other => (other / 2).max(1),
    }
}

fn beat_length_at(chart: &Chart, time: f64) -> f64 {
    if chart.bpm.is_empty() {
        return 500.0;
    }
    let mut current = chart.bpm[0].data.ms_per_beat;
    for item in &chart.bpm {
        if item.time > time {
            break;
        }
        current = item.data.ms_per_beat;
    }
    current
}

pub fn detect_direction(previous_row: &[usize], current_row: &[usize]) -> (Direction, bool) {
    let pleftmost = previous_row[0];
    let prightmost = previous_row[previous_row.len() - 1];
    let cleftmost = current_row[0];
    let crightmost = current_row[current_row.len() - 1];

    let leftmost_change = cleftmost as i64 - pleftmost as i64;
    let rightmost_change = crightmost as i64 - prightmost as i64;

    let mut direction = Direction::None;
    if leftmost_change > 0 {
        direction = if rightmost_change > 0 {
            Direction::Right
        } else {
            Direction::Inwards
        };
    } else if leftmost_change < 0 {
        direction = if rightmost_change < 0 {
            Direction::Left
        } else {
            Direction::Outwards
        };
    } else if rightmost_change < 0 {
        direction = Direction::Inwards;
    } else if rightmost_change > 0 {
        direction = Direction::Outwards;
    }

    let is_roll = pleftmost > crightmost || prightmost < cleftmost;
    (direction, is_roll)
}

pub fn calculate_primitives(chart: &Chart) -> Vec<Primitive> {
    let first_note = chart.notes[0].time;
    let first_row = &chart.notes[0].data;

    let mut previous_row: Vec<usize> = Vec::new();
    for (column, note) in first_row.iter().enumerate() {
        if matches!(note, NoteType::Normal | NoteType::HoldHead) {
            previous_row.push(column);
        }
    }

    if previous_row.is_empty() {
        return Vec::new();
    }

    let mut previous_time = first_note;
    let left_hand_keys = keys_on_left_hand(chart.keys);
    let mut out = Vec::new();

    // Row numbers start at 1: mania_map_analyser numbers every row after the first object, and skipped rows still take a number.
    for (offset, row) in chart.notes.iter().skip(1).enumerate() {
        let t = row.time;
        let index = offset + 1;

        let mut current_row = Vec::new();
        let mut normal_notes = Vec::new();
        let mut ln_heads = Vec::new();
        let mut ln_bodies = Vec::new();
        let mut ln_tails = Vec::new();

        for (column, note) in row.data.iter().enumerate() {
            match note {
                NoteType::Normal => {
                    current_row.push(column);
                    normal_notes.push(column);
                }
                NoteType::HoldHead => {
                    current_row.push(column);
                    ln_heads.push(column);
                }
                NoteType::HoldBody => ln_bodies.push(column),
                NoteType::HoldTail => ln_tails.push(column),
                NoteType::Nothing => {}
            }
        }

        if current_row.is_empty() && ln_bodies.is_empty() && ln_tails.is_empty() {
            continue;
        }

        let mut direction = Direction::None;
        let mut is_roll = false;
        let mut jacks = 0usize;

        if !current_row.is_empty() {
            (direction, is_roll) = detect_direction(&previous_row, &current_row);
            jacks = current_row
                .iter()
                .filter(|column| previous_row.contains(column))
                .count();
        }

        // The gap between adjacent rows is converted to a 1/4 beat value using the previous row time before it is updated.
        let ms_per_beat = (t - previous_time) * 4.0;

        if !current_row.is_empty() {
            previous_row = current_row.clone();
        }
        previous_time = t;

        out.push(Primitive {
            index,
            time: t - first_note,
            ms_per_beat,
            beat_length: beat_length_at(chart, t),
            notes: current_row.len(),
            jacks,
            direction,
            roll: is_roll,
            keys: chart.keys,
            left_hand_keys,
            ln_heads,
            ln_bodies,
            ln_tails,
            normal_notes,
            raw_notes: current_row,
        });
    }

    out
}

/// Share of short notes to hold heads; hold tails are not counted twice.
pub fn ln_percent(chart: &Chart) -> f64 {
    let mut notes = 0usize;
    let mut lnotes = 0usize;

    for row in &chart.notes {
        for note in &row.data {
            match note {
                NoteType::Normal => notes += 1,
                NoteType::HoldHead => {
                    notes += 1;
                    lnotes += 1;
                }
                _ => {}
            }
        }
    }

    if notes > 0 {
        lnotes as f64 / notes as f64
    } else {
        0.0
    }
}

fn is_non_one_velocity(velocity: f64) -> bool {
    !velocity.is_finite() || (velocity - 1.0).abs() > SV_SPEED_EPS
}

pub fn sv_time(chart: &Chart) -> f64 {
    if chart.sv.is_empty() {
        return 0.0;
    }

    let mut total = 0.0;
    let mut time = chart.first_note();
    let mut velocity = 1.0;
    let mut non_one_intervals = 0usize;
    let mut in_non_one = false;

    for item in &chart.sv {
        let current_velocity = item.data;
        let current_non_one = is_non_one_velocity(current_velocity);

        if is_non_one_velocity(velocity) {
            total += item.time - time;
        }

        if current_non_one && !in_non_one {
            non_one_intervals += 1;
            in_non_one = true;
        } else if !current_non_one {
            in_non_one = false;
        }

        velocity = current_velocity;
        time = item.time;
    }

    if is_non_one_velocity(velocity) {
        total += chart.last_note() - time;
    }

    if non_one_intervals <= 1 {
        return 0.0;
    }

    let mut extreme = false;
    if !chart.bpm.is_empty() {
        let mut previous_ms_per_beat: Option<f64> = None;
        for item in &chart.bpm {
            let ms_per_beat = item.data.ms_per_beat;
            if !ms_per_beat.is_finite() || ms_per_beat <= 0.0 {
                extreme = true;
                break;
            }

            let bpm = 60000.0 / ms_per_beat;
            if bpm <= SV_EXTREME_BPM_MIN || bpm >= SV_EXTREME_BPM_MAX {
                extreme = true;
                break;
            }

            if let Some(previous) = previous_ms_per_beat.filter(|value| *value > 0.0) {
                let ratio = (previous / ms_per_beat).max(ms_per_beat / previous);
                if ratio >= SV_EXTREME_BPM_RATIO {
                    extreme = true;
                    break;
                }
            }

            previous_ms_per_beat = Some(ms_per_beat);
        }
    }

    if extreme {
        return total.max(super::config::SV_AMOUNT_THRESHOLD + 1.0);
    }

    total
}
