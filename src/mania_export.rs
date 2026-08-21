use std::{fs, path::Path, sync::Arc};

use anyhow::Result;
use arrow_array::{ArrayRef, Float32Array, Int64Array, RecordBatch, UInt8Array};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;

use crate::{MANIA_DIFFICULTY_DIMENSIONS, MANIA_STYLE_DIMENSIONS, ManiaFeatureRecord};

const DIFFICULTY_NAMES: [&str; MANIA_DIFFICULTY_DIMENSIONS] = [
    "speed",
    "hand_stream",
    "jack",
    "chordjack",
    "technical",
    "stamina",
    "long_note",
    "course",
];

const STYLE_NAMES: [&str; MANIA_STYLE_DIMENSIONS] = [
    "stream_share",
    "chordstream_share",
    "jacks_share",
    "coordination_share",
    "density_share",
    "wildcard_share",
    "chord_rate",
    "large_chord_rate",
    "rotation_rate",
    "anchor_rate",
    "rhythm_entropy",
    "transition_entropy",
    "ln_note_ratio",
    "hold_occupancy",
    "hybrid_row_ratio",
    "peak_to_sustain_gap",
];

pub fn export_mania_csv(path: impl AsRef<Path>, records: &[ManiaFeatureRecord]) -> Result<()> {
    let mut output = String::from(
        "beatmap_id,beatmapset_id,key_count,mode_family,dominant_pattern,difficulty_percentile,difficulty_band",
    );
    for name in DIFFICULTY_NAMES {
        output.push(',');
        output.push_str(name);
    }
    for name in STYLE_NAMES {
        output.push(',');
        output.push_str(name);
    }
    output.push_str(",bpm,length_seconds,active_length_seconds,note_count,row_count,avg_nps,peak_nps,break_density,sv_change_rate\n");
    for record in records {
        output.push_str(&format!(
            "{},{},{},{},{},{},{}",
            record.beatmap_id,
            record.beatmapset_id,
            record.key_count,
            record.mode_family.as_str(),
            record.dominant_pattern.as_str(),
            record.difficulty_percentile,
            record.difficulty_band,
        ));
        for value in record.difficulty.as_array() {
            output.push_str(&format!(",{value}"));
        }
        for value in record.style.as_array() {
            output.push_str(&format!(",{value}"));
        }
        let base = record.base;
        output.push_str(&format!(
            ",{},{},{},{},{},{},{},{},{}\n",
            base.bpm,
            base.length_seconds,
            base.active_length_seconds,
            base.note_count,
            base.row_count,
            base.avg_nps,
            base.peak_nps,
            base.break_density,
            base.sv_change_rate,
        ));
    }
    fs::write(path, output)?;
    Ok(())
}

pub fn export_mania_parquet(path: impl AsRef<Path>, records: &[ManiaFeatureRecord]) -> Result<()> {
    let mut fields = vec![
        Field::new("beatmap_id", DataType::Int64, false),
        Field::new("beatmapset_id", DataType::Int64, false),
        Field::new("key_count", DataType::UInt8, false),
        Field::new("mode_family", DataType::UInt8, false),
        Field::new("dominant_pattern", DataType::UInt8, false),
        Field::new("difficulty_percentile", DataType::Float32, false),
        Field::new("difficulty_band", DataType::UInt8, false),
    ];
    fields.extend(
        DIFFICULTY_NAMES
            .iter()
            .map(|name| Field::new(*name, DataType::Float32, false)),
    );
    fields.extend(
        STYLE_NAMES
            .iter()
            .map(|name| Field::new(*name, DataType::Float32, false)),
    );
    for name in [
        "bpm",
        "length_seconds",
        "active_length_seconds",
        "note_count",
        "row_count",
        "avg_nps",
        "peak_nps",
        "break_density",
        "sv_change_rate",
    ] {
        fields.push(Field::new(name, DataType::Float32, false));
    }
    let schema = Arc::new(Schema::new(fields));
    let mut arrays: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(
            records.iter().map(|record| record.beatmap_id as i64),
        )),
        Arc::new(Int64Array::from_iter_values(
            records.iter().map(|record| record.beatmapset_id as i64),
        )),
        Arc::new(UInt8Array::from_iter_values(
            records.iter().map(|record| record.key_count),
        )),
        Arc::new(UInt8Array::from_iter_values(
            records.iter().map(|record| record.mode_family as u8),
        )),
        Arc::new(UInt8Array::from_iter_values(
            records.iter().map(|record| record.dominant_pattern as u8),
        )),
        Arc::new(Float32Array::from_iter_values(
            records.iter().map(|record| record.difficulty_percentile),
        )),
        Arc::new(UInt8Array::from_iter_values(
            records.iter().map(|record| record.difficulty_band),
        )),
    ];
    for index in 0..MANIA_DIFFICULTY_DIMENSIONS {
        arrays.push(Arc::new(Float32Array::from_iter_values(
            records
                .iter()
                .map(move |record| record.difficulty.as_array()[index]),
        )));
    }
    for index in 0..MANIA_STYLE_DIMENSIONS {
        arrays.push(Arc::new(Float32Array::from_iter_values(
            records
                .iter()
                .map(move |record| record.style.as_array()[index]),
        )));
    }
    let base_column = |selector: fn(&ManiaFeatureRecord) -> f32| {
        Arc::new(Float32Array::from_iter_values(records.iter().map(selector))) as ArrayRef
    };
    arrays.extend([
        base_column(|record| record.base.bpm),
        base_column(|record| record.base.length_seconds),
        base_column(|record| record.base.active_length_seconds),
        base_column(|record| record.base.note_count),
        base_column(|record| record.base.row_count),
        base_column(|record| record.base.avg_nps),
        base_column(|record| record.base.peak_nps),
        base_column(|record| record.base.break_density),
        base_column(|record| record.base.sv_change_rate),
    ]);
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    let mut writer = ArrowWriter::try_new(fs::File::create(path)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
