//! Chart structures.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteType {
    Nothing,
    Normal,
    HoldHead,
    HoldBody,
    HoldTail,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub time: f64,
    pub data: Vec<NoteType>,
}

impl Row {
    pub fn new(time: f64, keys: usize) -> Self {
        Self {
            time,
            data: vec![NoteType::Nothing; keys],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TimeItem<T> {
    pub time: f64,
    pub data: T,
}

#[derive(Debug, Clone, Copy)]
pub struct BpmData {
    pub meter: i64,
    pub ms_per_beat: f64,
}

#[derive(Debug, Clone)]
pub struct Chart {
    pub keys: usize,
    pub notes: Vec<Row>,
    pub bpm: Vec<TimeItem<BpmData>>,
    pub sv: Vec<TimeItem<f64>>,
}

impl Chart {
    pub fn first_note(&self) -> f64 {
        self.notes[0].time
    }

    pub fn last_note(&self) -> f64 {
        self.notes[self.notes.len() - 1].time
    }

    pub fn duration(&self) -> f64 {
        self.last_note() - self.first_note()
    }
}
