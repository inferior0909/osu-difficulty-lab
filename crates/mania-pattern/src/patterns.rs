//! Pattern definitions and matching.
//!
//! Matchers only read the first rows of a window, so windows are passed as fixed-size slices.

use super::chart::Chart;
use super::config::{
    COORDINATION_SPECIFIC_ORDER, DENSITY_SPECIFIC_ORDER, ENABLE_MULTI_LABEL_SAME_WINDOW,
    INVERSE_GAP_TOLERANCE_MS, INVERSE_MIN_FILLED_LANES, JACKY_CONTEXT_WINDOW,
    JACKY_FALLBACK_MAX_MSPB, JACKY_MIN_BPM, PATTERN_STABILITY_THRESHOLD, RC_CORE_LN_SCALE,
    RC_LN_CORE_SCALE, RELEASE_FULL_MATCH_ROWS, RELEASE_MIN_TAIL_ROWS, RELEASE_ROLL_POINTS,
    RELEASE_SCAN_ROWS, SHIELD_MAX_BEAT_RATIO, WILDCARD_SPECIFIC_ORDER, core_rating_multiplier,
    subtype_rating_multiplier,
};
use super::primitives::{Direction, Primitive, calculate_primitives, detect_direction};

const MATCHER_WINDOW: usize = 8;

pub const CORE_STREAM_NAME: &str = "Stream";
pub const CORE_CHORDSTREAM_NAME: &str = "Chordstream";
pub const CORE_JACKS_NAME: &str = "Jacks";
pub const CORE_COORDINATION_NAME: &str = "Coordination";
pub const CORE_DENSITY_NAME: &str = "Density";
pub const CORE_WILDCARD_NAME: &str = "Wildcard";

#[derive(Debug, Clone)]
pub struct FoundPattern {
    pub pattern: &'static str,
    pub specific_type: Option<&'static str>,
    pub mixed: bool,
    pub start: f64,
    pub end: f64,
    pub ms_per_beat: f64,
}

/// Matcher signature: a non-zero result is the number of matched rows.
pub type Matcher = fn(&[Primitive]) -> usize;

pub struct SpecificTable {
    pub stream: Vec<(&'static str, Matcher)>,
    pub chordstream: Vec<(&'static str, Matcher)>,
    pub jack: Vec<(&'static str, Matcher)>,
    pub coordination: Vec<(&'static str, Matcher)>,
    pub density: Vec<(&'static str, Matcher)>,
    pub wildcard: Vec<(&'static str, Matcher)>,
}

impl SpecificTable {
    fn for_core(&self, pattern: &str) -> &[(&'static str, Matcher)] {
        match pattern {
            CORE_STREAM_NAME => &self.stream,
            CORE_CHORDSTREAM_NAME => &self.chordstream,
            CORE_JACKS_NAME => &self.jack,
            CORE_COORDINATION_NAME => &self.coordination,
            CORE_DENSITY_NAME => &self.density,
            CORE_WILDCARD_NAME => &self.wildcard,
            _ => &[],
        }
    }
}

pub fn resolve_rating_multiplier(
    pattern: &str,
    specific_type: Option<&str>,
    mode_tag: &str,
) -> f64 {
    let default_multiplier = core_rating_multiplier(pattern);
    let is_ln_core = matches!(
        pattern,
        CORE_COORDINATION_NAME | CORE_DENSITY_NAME | CORE_WILDCARD_NAME
    );
    let is_rc_core = matches!(
        pattern,
        CORE_STREAM_NAME | CORE_CHORDSTREAM_NAME | CORE_JACKS_NAME
    );

    let mut value = match specific_type {
        None => default_multiplier,
        Some(name) => subtype_rating_multiplier(mode_tag, name).unwrap_or(default_multiplier),
    };

    if mode_tag == "RC" && is_ln_core {
        let base = match specific_type {
            None => default_multiplier,
            Some(name) => subtype_rating_multiplier("Mix", name).unwrap_or(default_multiplier),
        };
        value = base * RC_LN_CORE_SCALE;
    }

    if mode_tag == "LN" && is_rc_core {
        value *= RC_CORE_LN_SCALE;
    }

    value
}

pub fn jack_bpm(delta_ms: f64) -> f64 {
    if delta_ms <= 0.0 {
        return 230.0;
    }
    (15000.0 / delta_ms).min(230.0)
}

fn is_same_hand_adjacent(col_a: usize, col_b: usize, split: usize) -> bool {
    if col_a.abs_diff(col_b) != 1 {
        return false;
    }
    (col_a < split) == (col_b < split)
}

fn as_head_point_row(row: &Primitive, previous_head_cols: &[usize]) -> Primitive {
    let head_cols = row.ln_heads.clone();
    let jacks = head_cols
        .iter()
        .filter(|column| previous_head_cols.contains(column))
        .count();

    let mut direction = Direction::None;
    let mut roll = false;
    if !previous_head_cols.is_empty() && !head_cols.is_empty() {
        (direction, roll) = detect_direction(previous_head_cols, &head_cols);
    }

    Primitive {
        index: row.index,
        time: row.time,
        ms_per_beat: row.ms_per_beat,
        beat_length: row.beat_length,
        notes: head_cols.len(),
        jacks,
        direction,
        roll,
        keys: row.keys,
        left_hand_keys: row.left_hand_keys,
        ln_heads: row.ln_heads.clone(),
        ln_bodies: row.ln_bodies.clone(),
        ln_tails: row.ln_tails.clone(),
        normal_notes: Vec::new(),
        raw_notes: head_cols,
    }
}

fn head_rows(xs: &[Primitive], n: usize) -> Vec<Primitive> {
    let mut rows = Vec::new();
    let mut prev: Vec<usize> = Vec::new();
    for row in xs.iter().take(n) {
        let head_row = as_head_point_row(row, &prev);
        if !head_row.raw_notes.is_empty() {
            prev = head_row.raw_notes.clone();
        }
        rows.push(head_row);
    }
    rows
}

fn is_ln_head_context(xs: &[Primitive]) -> bool {
    xs.first().is_some_and(|row| !row.ln_heads.is_empty())
}

fn has_ln_context(xs: &[Primitive], window: usize) -> bool {
    xs.iter().take(window).any(|row| {
        !row.ln_heads.is_empty() || !row.ln_bodies.is_empty() || !row.ln_tails.is_empty()
    })
}

fn inverse_ready(xs: &[Primitive]) -> bool {
    if xs.len() < 5 {
        return false;
    }
    let win = &xs[..5];
    if win.iter().any(|row| !row.normal_notes.is_empty()) {
        return false;
    }
    let max_bodies = win.iter().map(|row| row.ln_bodies.len()).max().unwrap_or(0);
    if max_bodies < INVERSE_MIN_FILLED_LANES {
        return false;
    }

    let mut gaps = Vec::new();
    for i in 0..win.len() - 1 {
        if !win[i].ln_tails.is_empty() && !win[i + 1].ln_heads.is_empty() {
            gaps.push(win[i + 1].time - win[i].time);
        }
    }
    if gaps.len() < 2 {
        return false;
    }
    let max = gaps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = gaps.iter().cloned().fold(f64::INFINITY, f64::min);
    (max - min) <= INVERSE_GAP_TOLERANCE_MS
}

// ---- Core patterns ----

pub fn core_stream(xs: &[Primitive]) -> usize {
    if xs.len() < 5 {
        return 0;
    }
    let row = &xs[..5];
    if row.iter().all(|x| x.notes == 1 && x.jacks == 0)
        && row[0].raw_notes[0] != row[4].raw_notes[0]
    {
        return 5;
    }
    0
}

pub fn core_jacks(xs: &[Primitive]) -> usize {
    match xs.first() {
        Some(row) if row.jacks > 1 && row.ms_per_beat < 2000.0 => 1,
        _ => 0,
    }
}

pub fn core_chordstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let row = &xs[..4];
    if row[0].notes > 1 && row.iter().all(|x| x.jacks == 0) && row[1..].iter().any(|x| x.notes > 1)
    {
        return 4;
    }
    0
}

pub fn core_coordination(xs: &[Primitive]) -> usize {
    match xs.first() {
        Some(row)
            if !row.ln_heads.is_empty()
                || !row.ln_bodies.is_empty()
                || !row.ln_tails.is_empty() =>
        {
            1
        }
        _ => 0,
    }
}

pub fn core_density(xs: &[Primitive]) -> usize {
    usize::from(is_ln_head_context(xs))
}

pub fn core_wildcard(xs: &[Primitive]) -> usize {
    usize::from(is_ln_head_context(xs))
}

// ---- Jacks ----

pub fn jacks_chordjacks(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    if a.notes > 2 && b.notes > 1 && b.jacks >= 1 && (b.notes < a.notes || b.jacks < b.notes) {
        return 2;
    }
    0
}

pub fn jacks_minijacks(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    if a.jacks > 0 && b.jacks == 0 { 2 } else { 0 }
}

pub fn jacks_longjacks(xs: &[Primitive]) -> usize {
    if xs.len() < 5 {
        return 0;
    }
    let row = &xs[..5];
    if row.iter().all(|x| x.jacks > 0)
        && row[0].raw_notes.iter().any(|column| {
            row[1..]
                .iter()
                .all(|other| other.raw_notes.contains(column))
        })
    {
        return 5;
    }
    0
}

pub fn jacks_4k_quadstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let (a, c, d) = (&xs[0], &xs[2], &xs[3]);
    if a.notes == 4 && c.jacks == 0 && d.jacks == 0 {
        4
    } else {
        0
    }
}

pub fn jacks_4k_gluts(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let (a, b, c) = (&xs[0], &xs[1], &xs[2]);
    if b.jacks == 1 && c.jacks == 1 {
        let shared = a
            .raw_notes
            .iter()
            .any(|column| b.raw_notes.contains(column) && c.raw_notes.contains(column));
        if !shared {
            return 3;
        }
    }
    0
}

// ---- Chordstream ----

pub fn chordstream_4k_handstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let row = &xs[..4];
    if row[0].notes == 3 && row.iter().all(|x| x.jacks == 0) {
        4
    } else {
        0
    }
}

pub fn chordstream_4k_jumpstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let (a, b, c, d) = (&xs[0], &xs[1], &xs[2], &xs[3]);
    if a.notes == 2
        && a.jacks == 0
        && b.notes == 1
        && b.jacks == 0
        && c.jacks == 0
        && d.jacks == 0
        && c.notes < 3
        && d.notes < 3
    {
        return 4;
    }
    0
}

// Exported by mania_map_analyser's `patternsDef.js` but unused by the 4K subtype table.
#[allow(dead_code)]
pub fn chordstream_4k_double_jumpstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let (a, b, c, d) = (&xs[0], &xs[1], &xs[2], &xs[3]);
    if a.notes == 1
        && a.jacks == 0
        && b.notes == 2
        && b.jacks == 0
        && c.notes == 2
        && c.jacks == 0
        && d.notes == 1
        && d.jacks == 0
    {
        return 4;
    }
    0
}

pub fn chordstream_4k_triple_jumpstream(xs: &[Primitive]) -> usize {
    if xs.len() < 5 {
        return 0;
    }
    let row = &xs[..5];
    let notes: Vec<usize> = row.iter().map(|x| x.notes).collect();
    if notes == [1, 2, 2, 2, 1] && row.iter().all(|x| x.jacks == 0) {
        return 4;
    }
    0
}

pub fn chordstream_4k_jumptrill(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let row = &xs[..4];
    if row.iter().all(|x| x.notes == 2) && row[1].roll && row[2].roll && row[3].roll {
        4
    } else {
        0
    }
}

pub fn chordstream_4k_splittrill(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let (a, b, c) = (&xs[0], &xs[1], &xs[2]);
    if a.notes == 2
        && b.notes == 2
        && c.notes == 2
        && b.jacks == 0
        && c.jacks == 0
        && !b.roll
        && !c.roll
    {
        return 3;
    }
    0
}

// ---- Stream ----

pub fn stream_4k_roll(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let (a, b, c) = (&xs[0], &xs[1], &xs[2]);
    if a.notes == 1 && b.notes == 1 && c.notes == 1 {
        let left = a.direction == Direction::Left
            && b.direction == Direction::Left
            && c.direction == Direction::Left;
        let right = a.direction == Direction::Right
            && b.direction == Direction::Right
            && c.direction == Direction::Right;
        if left || right {
            return 3;
        }
    }
    0
}

pub fn stream_4k_trill(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let (a, b, c, d) = (&xs[0], &xs[1], &xs[2], &xs[3]);
    if b.jacks == 0
        && c.jacks == 0
        && d.jacks == 0
        && a.raw_notes == c.raw_notes
        && b.raw_notes == d.raw_notes
    {
        return 4;
    }
    0
}

pub fn stream_4k_minitrill(xs: &[Primitive]) -> usize {
    if xs.len() < 4 {
        return 0;
    }
    let (a, b, c, d) = (&xs[0], &xs[1], &xs[2], &xs[3]);
    if b.jacks == 0 && c.jacks == 0 && a.raw_notes == c.raw_notes && b.raw_notes != d.raw_notes {
        return 4;
    }
    0
}

// ---- Chordstream outside 4K ----

pub fn chordstream_7k_double_streams(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    if a.notes == 2 && b.notes == 2 && b.jacks == 0 && !b.roll {
        2
    } else {
        0
    }
}

pub fn chordstream_7k_dense_chordstream(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    if a.notes > 1 && b.notes > 1 && b.jacks == 0 {
        2
    } else {
        0
    }
}

pub fn chordstream_7k_light_chordstream(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    if a.notes > 1 && b.notes == 1 && b.jacks == 0 {
        2
    } else {
        0
    }
}

pub fn chordstream_7k_chord_roll(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let (a, b, c) = (&xs[0], &xs[1], &xs[2]);
    if a.notes > 1 && b.notes > 1 && c.notes > 1 && b.roll && c.roll {
        let left = b.direction == Direction::Left && c.direction == Direction::Left;
        let right = b.direction == Direction::Right && c.direction == Direction::Right;
        if left || right {
            return 3;
        }
    }
    0
}

pub fn chordstream_7k_brackets(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let (a, b, c) = (&xs[0], &xs[1], &xs[2]);
    if a.notes > 2
        && b.notes > 2
        && c.notes > 2
        && !b.roll
        && !c.roll
        && b.jacks == 0
        && c.jacks == 0
        && (a.notes + b.notes + c.notes) > 9
    {
        return 3;
    }
    0
}

// ---- Coordination ----

pub fn coordination_column_lock(xs: &[Primitive]) -> usize {
    if xs.len() < 3 {
        return 0;
    }
    let split = xs[0].left_hand_keys;
    let Some(ln_col) = xs[0].ln_heads.first().copied() else {
        return 0;
    };

    let adjacent: Vec<usize> = [ln_col.checked_sub(1), Some(ln_col + 1)]
        .into_iter()
        .flatten()
        .filter(|column| *column < xs[0].keys && is_same_hand_adjacent(ln_col, *column, split))
        .collect();
    if adjacent.is_empty() {
        return 0;
    }

    for adj in adjacent {
        let mut hits = Vec::new();
        for row in xs.iter().take(8) {
            if row.ln_bodies.contains(&ln_col) && row.normal_notes.contains(&adj) {
                hits.push(row.time);
            }
        }
        if hits.len() < 3 {
            continue;
        }

        let mut bpms = Vec::new();
        for pair in hits.windows(2) {
            bpms.push(jack_bpm(pair[1] - pair[0]));
        }
        let max = bpms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if !bpms.is_empty() && max >= JACKY_MIN_BPM {
            return 3;
        }
    }

    0
}

pub fn coordination_shield(xs: &[Primitive]) -> usize {
    if xs.len() < 2 {
        return 0;
    }
    let (a, b) = (&xs[0], &xs[1]);
    let dt = b.time - a.time;
    let beat_limit = b.beat_length * SHIELD_MAX_BEAT_RATIO;
    if dt < 0.0 || dt > beat_limit {
        return 0;
    }

    if a.normal_notes
        .iter()
        .any(|column| b.ln_heads.contains(column))
    {
        return 2;
    }
    if a.ln_tails
        .iter()
        .any(|column| b.normal_notes.contains(column))
    {
        return 2;
    }
    0
}

pub fn coordination_release(xs: &[Primitive]) -> usize {
    if xs.len() < RELEASE_MIN_TAIL_ROWS {
        return 0;
    }
    if coordination_shield(xs) != 0 {
        return 0;
    }
    if inverse_ready(xs) {
        return 0;
    }
    if wildcard_jack(xs) != 0 {
        return 0;
    }

    let picked: Vec<&Primitive> = xs
        .iter()
        .take(RELEASE_SCAN_ROWS)
        .filter(|row| row.ln_tails.len() == 1)
        .collect();
    if picked.len() < RELEASE_MIN_TAIL_ROWS {
        return 0;
    }

    let use_rows = RELEASE_FULL_MATCH_ROWS.min(picked.len());
    let tails: Vec<usize> = picked
        .iter()
        .take(use_rows)
        .map(|row| row.ln_tails[0])
        .collect();

    let mut prev = vec![tails[0]];
    let mut rows: Vec<Primitive> = Vec::with_capacity(use_rows);
    for index in 0..use_rows {
        let row = picked[index];
        let cur = vec![tails[index]];
        let (direction, roll) = detect_direction(&prev, &cur);
        rows.push(Primitive {
            index: row.index,
            time: row.time,
            ms_per_beat: row.ms_per_beat,
            beat_length: row.beat_length,
            notes: 1,
            jacks: usize::from(cur[0] == prev[0]),
            direction,
            roll,
            keys: row.keys,
            left_hand_keys: row.left_hand_keys,
            ln_heads: row.ln_heads.clone(),
            ln_bodies: row.ln_bodies.clone(),
            ln_tails: row.ln_tails.clone(),
            normal_notes: Vec::new(),
            raw_notes: cur.clone(),
        });
        prev = cur;
    }

    let effective_rows: &[Primitive] = if rows.len() > 1 { &rows[1..] } else { &[] };
    if effective_rows.len() < RELEASE_ROLL_POINTS {
        return 0;
    }

    let matched = if RELEASE_ROLL_POINTS >= 3 {
        stream_4k_roll(&effective_rows[..RELEASE_ROLL_POINTS]) != 0
    } else {
        let a = effective_rows[0].raw_notes[0];
        let b = effective_rows.get(1).map_or(a, |row| row.raw_notes[0]);
        let dt = effective_rows
            .get(1)
            .map_or(0.0, |row| row.time - effective_rows[0].time);
        a != b && dt > 0.0
    };

    if matched {
        return if use_rows >= RELEASE_FULL_MATCH_ROWS {
            5
        } else {
            4
        };
    }
    0
}

// ---- Density ----

pub fn density_4k_jumpstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 || !is_ln_head_context(xs) {
        return 0;
    }
    usize::from(chordstream_4k_jumpstream(&head_rows(xs, 4)) != 0) * 4
}

pub fn density_4k_handstream(xs: &[Primitive]) -> usize {
    if xs.len() < 4 || !is_ln_head_context(xs) {
        return 0;
    }
    usize::from(chordstream_4k_handstream(&head_rows(xs, 4)) != 0) * 4
}

pub fn density_4k_inverse(xs: &[Primitive]) -> usize {
    if inverse_ready(xs) { 5 } else { 0 }
}

pub fn density_7k_double_streams(xs: &[Primitive]) -> usize {
    if xs.len() < 2 || !is_ln_head_context(xs) {
        return 0;
    }
    usize::from(chordstream_7k_double_streams(&head_rows(xs, 2)) != 0) * 2
}

pub fn density_7k_dense_chordstream(xs: &[Primitive]) -> usize {
    if xs.len() < 2 || !is_ln_head_context(xs) {
        return 0;
    }
    usize::from(chordstream_7k_dense_chordstream(&head_rows(xs, 2)) != 0) * 2
}

pub fn density_7k_light_chordstream(xs: &[Primitive]) -> usize {
    if xs.len() < 2 || !is_ln_head_context(xs) {
        return 0;
    }
    usize::from(chordstream_7k_light_chordstream(&head_rows(xs, 2)) != 0) * 2
}

// ---- Wildcard ----

pub fn wildcard_jack(xs: &[Primitive]) -> usize {
    if xs.len() < 2 || !has_ln_context(xs, JACKY_CONTEXT_WINDOW) {
        return 0;
    }

    let rows: Vec<Primitive> = xs
        .iter()
        .take(JACKY_CONTEXT_WINDOW.max(4))
        .filter(|row| row.notes > 0)
        .cloned()
        .collect();
    if rows.len() < 2 {
        return 0;
    }

    if jacks_chordjacks(&rows) != 0 || jacks_minijacks(&rows) != 0 {
        return 4;
    }

    let check_rows = &rows[..rows.len().min(4)];
    let jack_rows = check_rows.iter().filter(|row| row.jacks > 0).count();
    if jack_rows >= 2 && check_rows.iter().any(|row| row.notes >= 2) {
        return 3;
    }

    let fastest = check_rows
        .iter()
        .map(|row| row.ms_per_beat)
        .fold(f64::INFINITY, f64::min);
    if jack_rows >= 2 && fastest <= JACKY_FALLBACK_MAX_MSPB {
        return 3;
    }
    0
}

pub fn wildcard_speed(xs: &[Primitive]) -> usize {
    if xs.len() < 2 || !has_ln_context(xs, 4) {
        return 0;
    }

    let rows = head_rows(xs, xs.len().min(4));
    if xs[0].keys == 4 {
        if rows.len() >= 3 && stream_4k_roll(&rows[..3]) != 0 {
            return 3;
        }
        if rows.len() >= 2 {
            let same_direction = matches!(rows[0].direction, Direction::Left | Direction::Right)
                && rows[0].direction == rows[1].direction;
            if same_direction || rows[0].ms_per_beat <= 180.0 {
                return 3;
            }
        }
    } else {
        if rows.len() >= 3 && chordstream_7k_chord_roll(&rows[..3]) != 0 {
            return 3;
        }
        if rows.len() >= 2 {
            let cond = rows[0].notes >= 2
                && rows[1].notes >= 2
                && rows[0].direction == rows[1].direction
                && matches!(rows[0].direction, Direction::Left | Direction::Right);
            if cond || rows[0].ms_per_beat <= 170.0 {
                return 3;
            }
        }
    }
    0
}

// ---- Subtype tables ----

/// Stable sort by preference order; unlisted names last.
fn reorder_specific(
    items: Vec<(&'static str, Matcher)>,
    preferred: &[&str],
) -> Vec<(&'static str, Matcher)> {
    if items.len() <= 1 || preferred.is_empty() {
        return items;
    }
    let rank = |name: &str| preferred.iter().position(|item| *item == name);
    let mut indexed: Vec<(usize, (&'static str, Matcher))> =
        items.into_iter().enumerate().collect();
    indexed.sort_by_key(|(index, (name, _))| (rank(name).unwrap_or(preferred.len()), *index));
    indexed.into_iter().map(|(_, item)| item).collect()
}

pub fn specific_4k() -> SpecificTable {
    SpecificTable {
        stream: vec![
            ("Rolls", stream_4k_roll as Matcher),
            ("Trills", stream_4k_trill as Matcher),
            ("Minitrills", stream_4k_minitrill as Matcher),
        ],
        chordstream: vec![
            ("Handstream", chordstream_4k_handstream as Matcher),
            ("Split Trill", chordstream_4k_splittrill as Matcher),
            ("Jumptrill", chordstream_4k_jumptrill as Matcher),
            ("Jumpstream", chordstream_4k_jumpstream as Matcher),
        ],
        jack: vec![
            ("Longjacks", jacks_longjacks as Matcher),
            ("Quadstream", jacks_4k_quadstream as Matcher),
            ("Gluts", jacks_4k_gluts as Matcher),
            ("Chordjacks", jacks_chordjacks as Matcher),
            ("Minijacks", jacks_minijacks as Matcher),
        ],
        coordination: coordination_table(),
        density: reorder_specific(
            vec![
                ("JS Density", density_4k_jumpstream as Matcher),
                ("HS Density", density_4k_handstream as Matcher),
                ("Inverse", density_4k_inverse as Matcher),
            ],
            &DENSITY_SPECIFIC_ORDER,
        ),
        wildcard: wildcard_table(),
    }
}

pub fn specific_7k() -> SpecificTable {
    SpecificTable {
        stream: Vec::new(),
        chordstream: vec![
            ("Brackets", chordstream_7k_brackets as Matcher),
            ("Double Stream", chordstream_7k_double_streams as Matcher),
            (
                "Dense Chordstream",
                chordstream_7k_dense_chordstream as Matcher,
            ),
            (
                "Light Chordstream",
                chordstream_7k_light_chordstream as Matcher,
            ),
        ],
        jack: vec![
            ("Longjacks", jacks_longjacks as Matcher),
            ("Chordjacks", jacks_chordjacks as Matcher),
            ("Minijacks", jacks_minijacks as Matcher),
        ],
        coordination: coordination_table(),
        density: reorder_specific(
            vec![
                ("DS Density", density_7k_double_streams as Matcher),
                ("DCS Density", density_7k_dense_chordstream as Matcher),
                ("LCS Density", density_7k_light_chordstream as Matcher),
                ("Inverse", density_4k_inverse as Matcher),
            ],
            &DENSITY_SPECIFIC_ORDER,
        ),
        wildcard: wildcard_table(),
    }
}

pub fn specific_other() -> SpecificTable {
    SpecificTable {
        stream: Vec::new(),
        chordstream: vec![
            ("Chord Rolls", chordstream_7k_chord_roll as Matcher),
            ("Double Stream", chordstream_7k_double_streams as Matcher),
            (
                "Dense Chordstream",
                chordstream_7k_dense_chordstream as Matcher,
            ),
            (
                "Light Chordstream",
                chordstream_7k_light_chordstream as Matcher,
            ),
        ],
        jack: vec![
            ("Longjacks", jacks_longjacks as Matcher),
            ("Chordjacks", jacks_chordjacks as Matcher),
            ("Minijacks", jacks_minijacks as Matcher),
        ],
        coordination: coordination_table(),
        density: reorder_specific(
            vec![
                ("DS Density", density_7k_double_streams as Matcher),
                ("DCS Density", density_7k_dense_chordstream as Matcher),
                ("LCS Density", density_7k_light_chordstream as Matcher),
                ("Inverse", density_4k_inverse as Matcher),
            ],
            &DENSITY_SPECIFIC_ORDER,
        ),
        wildcard: wildcard_table(),
    }
}

fn coordination_table() -> Vec<(&'static str, Matcher)> {
    reorder_specific(
        vec![
            ("Column Lock", coordination_column_lock as Matcher),
            ("Release", coordination_release as Matcher),
            ("Shield", coordination_shield as Matcher),
        ],
        &COORDINATION_SPECIFIC_ORDER,
    )
}

fn wildcard_table() -> Vec<(&'static str, Matcher)> {
    reorder_specific(
        vec![
            ("Jacky WC", wildcard_jack as Matcher),
            ("Speedy WC", wildcard_speed as Matcher),
        ],
        &WILDCARD_SPECIFIC_ORDER,
    )
}

pub fn specific_table_for(keys: usize) -> SpecificTable {
    if keys == 4 {
        specific_4k()
    } else if keys == 7 {
        specific_7k()
    } else {
        specific_other()
    }
}

// ---- Matching loop ----

fn resolved_mspb(pattern: &str, specific_type: Option<&str>, mean_ms_per_beat: f64) -> f64 {
    if pattern == CORE_DENSITY_NAME && specific_type == Some("Inverse") {
        return 0.0;
    }
    mean_ms_per_beat
}

fn append_found_pattern(
    results: &mut Vec<FoundPattern>,
    pattern: &'static str,
    specific_type: Option<&'static str>,
    n2: usize,
    remaining: &[Primitive],
    last_note: f64,
) {
    let d = &remaining[..n2.min(remaining.len())];
    let mean = d.iter().map(|row| row.ms_per_beat).sum::<f64>() / d.len() as f64;
    let mixed = !d
        .iter()
        .all(|row| (row.ms_per_beat - mean).abs() < PATTERN_STABILITY_THRESHOLD);

    let start = remaining[0].time;
    let end_candidate = if n2 < remaining.len() {
        remaining[n2].time
    } else {
        last_note
    };
    let end = if pattern == CORE_JACKS_NAME {
        (remaining[0].time + remaining[0].ms_per_beat * 0.5).max(end_candidate)
    } else {
        end_candidate
    };

    results.push(FoundPattern {
        pattern,
        specific_type,
        mixed,
        start,
        end,
        ms_per_beat: resolved_mspb(pattern, specific_type, mean),
    });
}

fn append_core_matches(
    results: &mut Vec<FoundPattern>,
    pattern: &'static str,
    core_n: usize,
    specific_list: &[(&'static str, Matcher)],
    remaining: &[Primitive],
    last_note: f64,
) {
    if core_n == 0 {
        return;
    }

    if ENABLE_MULTI_LABEL_SAME_WINDOW {
        let matched: Vec<(usize, &'static str)> = specific_list
            .iter()
            .filter_map(|(name, matcher)| {
                let n = matcher(remaining);
                (n != 0).then_some((n, *name))
            })
            .collect();
        if matched.is_empty() {
            append_found_pattern(results, pattern, None, core_n, remaining, last_note);
            return;
        }
        for (m, specific_type) in matched {
            append_found_pattern(
                results,
                pattern,
                Some(specific_type),
                core_n.max(m),
                remaining,
                last_note,
            );
        }
        return;
    }

    let picked = specific_list.iter().find_map(|(name, matcher)| {
        let n = matcher(remaining);
        (n != 0).then_some((n, *name))
    });
    match picked {
        None => append_found_pattern(results, pattern, None, core_n, remaining, last_note),
        Some((m, specific_type)) => append_found_pattern(
            results,
            pattern,
            Some(specific_type),
            core_n.max(m),
            remaining,
            last_note,
        ),
    }
}

fn matches(
    specific: &SpecificTable,
    last_note: f64,
    primitives: &[Primitive],
) -> Vec<FoundPattern> {
    let mut results = Vec::new();
    let mut start = 0usize;

    while start < primitives.len() {
        let remaining = &primitives[start..(start + MATCHER_WINDOW).min(primitives.len())];
        append_core_matches(
            &mut results,
            CORE_STREAM_NAME,
            core_stream(remaining),
            specific.for_core(CORE_STREAM_NAME),
            remaining,
            last_note,
        );
        append_core_matches(
            &mut results,
            CORE_CHORDSTREAM_NAME,
            core_chordstream(remaining),
            specific.for_core(CORE_CHORDSTREAM_NAME),
            remaining,
            last_note,
        );
        append_core_matches(
            &mut results,
            CORE_JACKS_NAME,
            core_jacks(remaining),
            specific.for_core(CORE_JACKS_NAME),
            remaining,
            last_note,
        );
        append_core_matches(
            &mut results,
            CORE_COORDINATION_NAME,
            core_coordination(remaining),
            specific.for_core(CORE_COORDINATION_NAME),
            remaining,
            last_note,
        );
        append_core_matches(
            &mut results,
            CORE_DENSITY_NAME,
            core_density(remaining),
            specific.for_core(CORE_DENSITY_NAME),
            remaining,
            last_note,
        );
        append_core_matches(
            &mut results,
            CORE_WILDCARD_NAME,
            core_wildcard(remaining),
            specific.for_core(CORE_WILDCARD_NAME),
            remaining,
            last_note,
        );

        start += 1;
    }

    results
}

pub fn find(chart: &Chart) -> Vec<FoundPattern> {
    let primitives = calculate_primitives(chart);
    let specific = specific_table_for(chart.keys);
    matches(
        &specific,
        chart.last_note() - chart.first_note(),
        &primitives,
    )
}
