//! `.osu` mania parsing.
//! Stacked objects are kept as-is, hold body/tail rows are derived from cross-column release times, and times are floored.

use anyhow::{Result, bail};

use super::chart::{BpmData, Chart, NoteType, Row, TimeItem};

enum HitObject {
    Circle { x: f64, time: f64 },
    Hold { x: f64, time: f64, end_time: f64 },
}

enum TimingPoint {
    Uninherited {
        time: f64,
        ms_per_beat: f64,
        meter: i64,
    },
    Inherited {
        time: f64,
        multiplier: f64,
    },
}

fn x_to_column(x: f64, keys: usize) -> usize {
    let column = ((x / 512.0) * keys as f64).trunc();
    if column < 0.0 {
        return 0;
    }
    let column = column as usize;
    column.min(keys - 1)
}

fn parse_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut current: Option<usize> = None;

    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].to_owned();
            sections.push((name, Vec::new()));
            current = Some(sections.len() - 1);
        } else if let Some(index) = current {
            sections[index].1.push(line.to_owned());
        }
    }

    sections
}

fn section<'a>(sections: &'a [(String, Vec<String>)], name: &str) -> &'a [String] {
    sections
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, lines)| lines.as_slice())
        .unwrap_or(&[])
}

fn parse_kv(lines: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in lines {
        if let Some(index) = line.find(':') {
            out.push((
                line[..index].trim().to_owned(),
                line[index + 1..].trim().to_owned(),
            ));
        }
    }
    out
}

fn kv<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn parse_timing_points(lines: &[String]) -> Vec<TimingPoint> {
    let mut out = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 2 {
            continue;
        }
        let time = parts[0].parse::<f64>().unwrap_or(f64::NAN);
        let beat_len = parts[1].parse::<f64>().unwrap_or(f64::NAN);
        let meter = match parts.get(2) {
            Some(value) if !value.is_empty() => value.parse::<i64>().unwrap_or(4),
            _ => 4,
        };
        let uninherited = match parts.get(6) {
            Some(value) if !value.is_empty() => value.parse::<i64>().unwrap_or(1),
            _ => 1,
        };

        if uninherited == 1 {
            out.push(TimingPoint::Uninherited {
                time,
                ms_per_beat: beat_len.max(0.0),
                meter,
            });
        } else if beat_len != 0.0 {
            out.push(TimingPoint::Inherited {
                time,
                multiplier: -100.0 / beat_len,
            });
        }
    }
    out
}

fn parse_hit_objects(lines: &[String]) -> Vec<HitObject> {
    let mut out = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 5 {
            continue;
        }
        let x = parts[0].parse::<f64>().unwrap_or(f64::NAN);
        let time = parts[2].parse::<f64>().unwrap_or(f64::NAN).trunc();
        let kind = parts[3].parse::<i64>().unwrap_or(0);

        if kind & 128 != 0 {
            let mut end_time = time;
            if let Some(value) = parts.get(5).filter(|value| !value.is_empty()) {
                let head = value.split(':').next().unwrap_or("");
                if let Ok(parsed) = head.parse::<f64>() {
                    end_time = parsed.trunc();
                }
            }
            out.push(HitObject::Hold { x, time, end_time });
        } else {
            out.push(HitObject::Circle { x, time });
        }
    }
    out
}

fn convert_hit_objects(objects: &[HitObject], keys: usize) -> Result<Vec<Row>> {
    let mut output: Vec<Row> = Vec::new();
    let mut holding_until: Vec<Option<f64>> = vec![None; keys];
    let mut last_row: Option<usize> = None;

    fn earliest_release(holding_until: &[Option<f64>]) -> f64 {
        holding_until
            .iter()
            .flatten()
            .fold(f64::INFINITY, |acc, value| acc.min(*value))
    }

    fn finish_holds(
        time: f64,
        keys: usize,
        holding_until: &mut [Option<f64>],
        output: &mut Vec<Row>,
        last_row: &mut Option<usize>,
    ) -> Result<()> {
        let mut earliest = earliest_release(holding_until);

        while earliest < time {
            for column in 0..keys {
                if holding_until[column] != Some(earliest) {
                    continue;
                }
                let needs_row = match *last_row {
                    Some(index) => earliest > output[index].time,
                    None => true,
                };
                if needs_row {
                    let mut row = Row::new(earliest, keys);
                    for (index, value) in holding_until.iter().enumerate() {
                        if value.is_some() {
                            row.data[index] = NoteType::HoldBody;
                        }
                    }
                    output.push(row);
                    *last_row = Some(output.len() - 1);
                }

                let index = last_row.expect("row exists after creation");
                let current = output[index].data[column];
                if current == NoteType::Nothing || current == NoteType::HoldBody {
                    output[index].data[column] = NoteType::HoldTail;
                    holding_until[column] = None;
                } else {
                    bail!("impossible (HOLDTAIL overwrite conflict)");
                }
            }
            earliest = earliest_release(holding_until);
        }
        Ok(())
    }

    fn open_row(
        time: f64,
        keys: usize,
        holding_until: &[Option<f64>],
        output: &mut Vec<Row>,
        last_row: &mut Option<usize>,
    ) {
        let needs_row = match *last_row {
            Some(index) => time > output[index].time,
            None => true,
        };
        if !needs_row {
            return;
        }
        let mut row = Row::new(time, keys);
        for (index, value) in holding_until.iter().enumerate() {
            if value.is_some() {
                row.data[index] = NoteType::HoldBody;
            }
        }
        output.push(row);
        *last_row = Some(output.len() - 1);
    }

    for object in objects {
        match *object {
            HitObject::Circle { x, time } => {
                finish_holds(time, keys, &mut holding_until, &mut output, &mut last_row)?;
                open_row(time, keys, &holding_until, &mut output, &mut last_row);
                let index = last_row.expect("row exists after open_row");
                let column = x_to_column(x, keys);
                match output[index].data[column] {
                    NoteType::Nothing => output[index].data[column] = NoteType::Normal,
                    // Stacked objects follow mania_map_analyser: a plain object in the same column and time is recorded once.
                    NoteType::Normal | NoteType::HoldHead => {}
                    other => bail!(
                        "Stacked note at {time}, column {}, coincides with {other:?}",
                        column + 1
                    ),
                }
            }
            HitObject::Hold { x, time, end_time } => {
                let column = x_to_column(x, keys);
                if end_time > time {
                    finish_holds(time, keys, &mut holding_until, &mut output, &mut last_row)?;
                    open_row(time, keys, &holding_until, &mut output, &mut last_row);
                    let index = last_row.expect("row exists after open_row");
                    match output[index].data[column] {
                        NoteType::Nothing | NoteType::Normal => {
                            output[index].data[column] = NoteType::HoldHead;
                            holding_until[column] = Some(end_time);
                        }
                        other => bail!(
                            "Stacked LN at {time}, column {}, head coincides with {other:?}",
                            column + 1
                        ),
                    }
                } else {
                    finish_holds(time, keys, &mut holding_until, &mut output, &mut last_row)?;
                    open_row(time, keys, &holding_until, &mut output, &mut last_row);
                    let index = last_row.expect("row exists after open_row");
                    match output[index].data[column] {
                        NoteType::Nothing => output[index].data[column] = NoteType::Normal,
                        NoteType::Normal | NoteType::HoldHead => {}
                        other => bail!(
                            "Stacked note at {time}, column {}, coincides with {other:?}",
                            column + 1
                        ),
                    }
                }
            }
        }
    }

    finish_holds(
        f64::INFINITY,
        keys,
        &mut holding_until,
        &mut output,
        &mut last_row,
    )?;
    Ok(output)
}

/// Total duration per BPM.
fn find_bpm_durations(points: &[TimingPoint], end_time: f64) -> Result<Vec<(f64, f64)>> {
    let uninherited: Vec<(f64, f64)> = points
        .iter()
        .filter_map(|point| match *point {
            TimingPoint::Uninherited {
                time, ms_per_beat, ..
            } => Some((time, ms_per_beat)),
            TimingPoint::Inherited { .. } => None,
        })
        .collect();

    if uninherited.is_empty() {
        bail!("Beatmap has no BPM points set");
    }

    let mut durations: Vec<(f64, f64)> = Vec::new();
    let mut current = uninherited[0].1;
    let mut time = uninherited[0].0;

    for (next_time, next_ms_per_beat) in uninherited.iter().skip(1) {
        match durations.iter_mut().find(|(key, _)| *key == current) {
            Some((_, total)) => *total += *next_time - time,
            None => durations.push((current, *next_time - time)),
        }
        time = *next_time;
        current = *next_ms_per_beat;
    }

    match durations.iter_mut().find(|(key, _)| *key == current) {
        Some((_, total)) => *total += (end_time - time).max(0.0),
        None => durations.push((current, (end_time - time).max(0.0))),
    }

    Ok(durations)
}

type ConvertedTiming = (Vec<TimeItem<BpmData>>, Vec<TimeItem<f64>>);

fn convert_timing_points(points: &[TimingPoint], end_time: f64) -> Result<ConvertedTiming> {
    let durations = find_bpm_durations(points, end_time)?;
    // mania_map_analyser sorts by total duration descending and takes the first entry, with a stable sort.
    let mut ordered = durations.clone();
    ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let most_common_mspb = ordered[0].0;

    let mut sv = Vec::new();
    let mut bpm = Vec::new();
    let mut current_bpm_mult = 1.0;

    for point in points {
        match *point {
            TimingPoint::Uninherited {
                time,
                ms_per_beat,
                meter,
            } => {
                bpm.push(TimeItem {
                    time,
                    data: BpmData { meter, ms_per_beat },
                });
                current_bpm_mult = if ms_per_beat != 0.0 {
                    most_common_mspb / ms_per_beat
                } else {
                    1.0
                };
                sv.push(TimeItem {
                    time,
                    data: current_bpm_mult,
                });
            }
            TimingPoint::Inherited { time, multiplier } => {
                sv.push(TimeItem {
                    time,
                    data: current_bpm_mult * multiplier,
                });
            }
        }
    }

    Ok((bpm, sv))
}

fn cleaned_sv(sv: &[TimeItem<f64>]) -> Vec<TimeItem<f64>> {
    if sv.is_empty() {
        return Vec::new();
    }

    let mut seen: Vec<f64> = Vec::new();
    let mut dedup_rev: Vec<TimeItem<f64>> = Vec::new();
    for item in sv.iter().rev() {
        if seen.contains(&item.time) {
            continue;
        }
        seen.push(item.time);
        dedup_rev.push(*item);
    }
    dedup_rev.reverse();

    let mut out = Vec::new();
    let mut previous = 1.0;
    for item in dedup_rev {
        if (item.data - previous).abs() > 0.005 {
            out.push(item);
            previous = item.data;
        }
    }
    out
}

pub fn parse_osu_mania(text: &str) -> Result<Chart> {
    let sections = parse_sections(text);
    let difficulty = parse_kv(section(&sections, "Difficulty"));
    let keys = kv(&difficulty, "CircleSize")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.trunc().max(1.0) as usize)
        .unwrap_or(4);

    let timing_points = parse_timing_points(section(&sections, "TimingPoints"));
    let hit_objects = parse_hit_objects(section(&sections, "HitObjects"));

    let notes = convert_hit_objects(&hit_objects, keys)?;
    if notes.is_empty() {
        bail!("Beatmap has no hitobjects after conversion");
    }

    let end_time = notes[notes.len() - 1].time;
    let (bpm, sv) = if timing_points.is_empty() {
        (
            vec![TimeItem {
                time: 0.0,
                data: BpmData {
                    meter: 4,
                    ms_per_beat: 500.0,
                },
            }],
            vec![TimeItem {
                time: 0.0,
                data: 1.0,
            }],
        )
    } else {
        let (bpm, sv) = convert_timing_points(&timing_points, end_time)?;
        (bpm, cleaned_sv(&sv))
    };

    Ok(Chart {
        keys,
        notes,
        bpm,
        sv,
    })
}
