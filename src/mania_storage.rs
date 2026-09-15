use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    MANIA_ANALYZER_ALGORITHM_ID, MANIA_ANALYZER_VERSION, MANIA_MMA_ALGORITHM_VERSION,
    MANIA_MMA_FEATURE_FILE, MANIA_RAW_FEATURE_FILE, ManiaBeatmapMetadata, ManiaFeatureRecord,
    ManiaGameMod, ManiaMmaRecord, ManiaModeFamily, ManiaPattern, ManiaRawFeatureRecord,
};

const RAW_HEADER: &[u8; 8] = b"ODLMAR1\0";
const NORMALIZED_HEADER: &[u8; 8] = b"ODLMAN1\0";
const MMA_HEADER: &[u8; 8] = b"ODLMMA1\0";

pub struct ManiaFeatureStore {
    root: PathBuf,
    connection: Connection,
    indexes_invalidated: bool,
}

impl ManiaFeatureStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("normalizers"))?;
        fs::create_dir_all(root.join("indexes"))?;
        let connection = Connection::open(root.join("mania-metadata.sqlite"))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS mania_beatmaps (
               beatmap_id INTEGER PRIMARY KEY,
               beatmapset_id INTEGER NOT NULL,
               checksum TEXT NOT NULL,
               artist TEXT NOT NULL,
               title TEXT NOT NULL,
               version TEXT NOT NULL,
               creator TEXT NOT NULL,
               online_url TEXT NOT NULL,
               key_count INTEGER NOT NULL,
               mode_family INTEGER NOT NULL,
               dominant_pattern INTEGER NOT NULL,
               updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS idx_mania_beatmaps_key_count
               ON mania_beatmaps(key_count);
             CREATE TABLE IF NOT EXISTS mania_analyses (
               beatmap_id INTEGER NOT NULL,
               analyzer_version INTEGER NOT NULL,
               normalization_version INTEGER NOT NULL DEFAULT 0,
               raw_offset INTEGER NOT NULL,
               normalized_offset INTEGER,
               status INTEGER NOT NULL,
               PRIMARY KEY (beatmap_id, analyzer_version),
               FOREIGN KEY (beatmap_id) REFERENCES mania_beatmaps(beatmap_id)
             );
             CREATE INDEX IF NOT EXISTS idx_mania_analyses_normalized
               ON mania_analyses(analyzer_version, normalization_version, status);
             CREATE TABLE IF NOT EXISTS mania_state (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mania_mma_analyses (
               beatmap_id INTEGER NOT NULL,
               game_mod TEXT NOT NULL,
               mma_version INTEGER NOT NULL,
               checksum TEXT NOT NULL,
               mma_offset INTEGER NOT NULL,
               status INTEGER NOT NULL,
               PRIMARY KEY (beatmap_id, game_mod, mma_version),
               FOREIGN KEY (beatmap_id) REFERENCES mania_beatmaps(beatmap_id)
             );
             CREATE INDEX IF NOT EXISTS idx_mania_mma_analyses_version
               ON mania_mma_analyses(mma_version, status);",
        )?;
        let store = Self {
            root,
            connection,
            indexes_invalidated: false,
        };
        store.ensure_header(MANIA_RAW_FEATURE_FILE, RAW_HEADER)?;
        store.ensure_header(MANIA_MMA_FEATURE_FILE, MMA_HEADER)?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn prepare_reanalysis(&mut self) -> Result<bool> {
        let snapshot = format!("{MANIA_ANALYZER_VERSION}:{MANIA_ANALYZER_ALGORITHM_ID}");
        let previous = self.state("analyzer_snapshot")?;
        if previous.as_deref() == Some(&snapshot) {
            return Ok(false);
        }
        self.set_state("analyzer_snapshot", &snapshot)?;
        self.ensure_indexes_invalidated()?;
        Ok(previous.is_some())
    }

    pub fn current_analysis_matches(&self, beatmap_id: u64, checksum: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM mania_analyses
               INNER JOIN mania_beatmaps USING(beatmap_id)
               WHERE beatmap_id=?1 AND analyzer_version=?2 AND status IN (1,2)
                 AND checksum=?3
             )",
            params![beatmap_id as i64, MANIA_ANALYZER_VERSION as i64, checksum],
            |row| row.get(0),
        )?)
    }

    pub fn has_current_analysis(&self, beatmap_id: u64) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM mania_analyses
              WHERE beatmap_id=?1 AND analyzer_version=?2 AND status IN (1,2))",
            params![beatmap_id as i64, MANIA_ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?)
    }

    pub fn append_raw(
        &mut self,
        metadata: &ManiaBeatmapMetadata,
        record: &ManiaRawFeatureRecord,
    ) -> Result<bool> {
        validate_raw(metadata, record)?;
        if self.current_analysis_matches(metadata.beatmap_id, &metadata.checksum)? {
            return Ok(false);
        }
        let offset = self.append_record(MANIA_RAW_FEATURE_FILE, RAW_HEADER, record)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO mania_beatmaps(
               beatmap_id,beatmapset_id,checksum,artist,title,version,creator,online_url,
               key_count,mode_family,dominant_pattern,updated_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,unixepoch())
             ON CONFLICT(beatmap_id) DO UPDATE SET
               beatmapset_id=excluded.beatmapset_id,checksum=excluded.checksum,
               artist=excluded.artist,title=excluded.title,version=excluded.version,
               creator=excluded.creator,online_url=excluded.online_url,
               key_count=excluded.key_count,mode_family=excluded.mode_family,
               dominant_pattern=excluded.dominant_pattern,updated_at=unixepoch()",
            params![
                metadata.beatmap_id as i64,
                metadata.beatmapset_id as i64,
                metadata.checksum,
                metadata.artist,
                metadata.title,
                metadata.version,
                metadata.creator,
                metadata.online_url,
                metadata.key_count as i64,
                metadata.mode_family as i64,
                metadata.dominant_pattern as i64,
            ],
        )?;
        transaction.execute(
            "INSERT INTO mania_analyses(
               beatmap_id,analyzer_version,normalization_version,raw_offset,normalized_offset,status
             ) VALUES(?1,?2,0,?3,NULL,1)
             ON CONFLICT(beatmap_id,analyzer_version) DO UPDATE SET
               normalization_version=0,raw_offset=excluded.raw_offset,
               normalized_offset=NULL,status=1",
            params![
                record.beatmap_id as i64,
                record.analyzer_version as i64,
                offset as i64,
            ],
        )?;
        transaction.commit()?;
        self.ensure_indexes_invalidated()?;
        Ok(true)
    }

    /// Key-pattern records are stored per beatmap and clock rate, so one beatmap has NM/DT/HT rows.
    pub fn current_mma_matches(
        &self,
        beatmap_id: u64,
        checksum: &str,
        game_mod: ManiaGameMod,
    ) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM mania_mma_analyses
              WHERE beatmap_id=?1 AND game_mod=?2 AND mma_version=?3 AND checksum=?4 AND status=1)",
            params![
                beatmap_id as i64,
                game_mod.as_str(),
                MANIA_MMA_ALGORITHM_VERSION as i64,
                checksum,
            ],
            |row| row.get(0),
        )?)
    }

    pub fn append_mma(&mut self, checksum: &str, record: &ManiaMmaRecord) -> Result<bool> {
        mania_pattern::validate_record(record)?;
        if self.current_mma_matches(record.beatmap_id, checksum, record.game_mod)? {
            return Ok(false);
        }
        let offset = self.append_record(MANIA_MMA_FEATURE_FILE, MMA_HEADER, record)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO mania_mma_analyses(
               beatmap_id,game_mod,mma_version,checksum,mma_offset,status
             ) VALUES(?1,?2,?3,?4,?5,1)
             ON CONFLICT(beatmap_id,game_mod,mma_version) DO UPDATE SET
               checksum=excluded.checksum,mma_offset=excluded.mma_offset,status=1",
            params![
                record.beatmap_id as i64,
                record.game_mod.as_str(),
                record.algorithm_version as i64,
                checksum,
                offset as i64,
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn mma_records(&self) -> Result<Vec<ManiaMmaRecord>> {
        // Only return records whose checksum still matches the current beatmap, so a replaced file cannot read old patterns.
        let offsets = self.offsets(
            "SELECT a.mma_offset FROM mania_mma_analyses a
             JOIN mania_beatmaps b ON b.beatmap_id = a.beatmap_id
             WHERE a.mma_version=?1 AND a.status=1 AND a.checksum = b.checksum
             ORDER BY a.beatmap_id, a.game_mod",
            &[MANIA_MMA_ALGORITHM_VERSION as i64],
        )?;
        offsets
            .into_iter()
            .map(|offset| self.read_record(MANIA_MMA_FEATURE_FILE, MMA_HEADER, offset))
            .collect()
    }

    pub fn mma_for_id(&self, beatmap_id: u64, game_mod: ManiaGameMod) -> Result<ManiaMmaRecord> {
        let offset: Option<u64> = self
            .connection
            .query_row(
                "SELECT a.mma_offset FROM mania_mma_analyses a
                 JOIN mania_beatmaps b ON a.beatmap_id=b.beatmap_id
                 WHERE a.beatmap_id=?1 AND a.game_mod=?2 AND a.mma_version=?3
                   AND a.status=1 AND a.checksum=b.checksum",
                params![
                    beatmap_id as i64,
                    game_mod.as_str(),
                    MANIA_MMA_ALGORITHM_VERSION as i64,
                ],
                |row| row.get(0),
            )
            .optional()?;
        let offset = offset.with_context(|| {
            format!(
                "no mania key-pattern record for beatmap {beatmap_id} {}",
                game_mod.as_str()
            )
        })?;
        self.read_record(MANIA_MMA_FEATURE_FILE, MMA_HEADER, offset)
    }

    pub fn mma_record_count(&self) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM mania_mma_analyses a JOIN mania_beatmaps b ON a.beatmap_id=b.beatmap_id
             WHERE a.mma_version=?1 AND a.status=1 AND a.checksum=b.checksum",
            params![MANIA_MMA_ALGORITHM_VERSION as i64],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    pub fn mma_record_count_for(&self, game_mod: ManiaGameMod) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM mania_mma_analyses a JOIN mania_beatmaps b ON a.beatmap_id=b.beatmap_id
             WHERE a.mma_version=?1 AND a.game_mod=?2 AND a.status=1 AND a.checksum=b.checksum",
            params![MANIA_MMA_ALGORITHM_VERSION as i64, game_mod.as_str()],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    pub fn raw_records(&self) -> Result<Vec<ManiaRawFeatureRecord>> {
        let offsets = self.offsets(
            "SELECT raw_offset FROM mania_analyses
             WHERE analyzer_version=?1 AND status IN (1,2) ORDER BY beatmap_id",
            &[MANIA_ANALYZER_VERSION as i64],
        )?;
        offsets
            .into_iter()
            .map(|offset| self.read_record(MANIA_RAW_FEATURE_FILE, RAW_HEADER, offset))
            .collect()
    }

    pub fn write_normalized(&mut self, version: u32, records: &[ManiaFeatureRecord]) -> Result<()> {
        if version == 0 {
            bail!("mania normalization version must be positive");
        }
        for record in records {
            validate_normalized(record, version)?;
        }
        let file_name = format!("mania-features-v{version}.bin");
        let path = self.root.join(&file_name);
        let temporary = path.with_extension("bin.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(NORMALIZED_HEADER)?;
        let mut offsets = Vec::with_capacity(records.len());
        for record in records {
            offsets.push(file.stream_position()?);
            bincode::serialize_into(&mut file, record)?;
        }
        file.sync_all()?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE mania_analyses SET normalization_version=0,normalized_offset=NULL,status=1
             WHERE analyzer_version=?1 AND status IN (1,2)",
            [MANIA_ANALYZER_VERSION as i64],
        )?;
        for (record, offset) in records.iter().zip(offsets) {
            let changed = transaction.execute(
                "UPDATE mania_analyses SET normalization_version=?1,normalized_offset=?2,status=2
                 WHERE beatmap_id=?3 AND analyzer_version=?4 AND status=1",
                params![
                    version as i64,
                    offset as i64,
                    record.beatmap_id as i64,
                    MANIA_ANALYZER_VERSION as i64,
                ],
            )?;
            if changed != 1 {
                bail!(
                    "mania normalized record {} has no active raw analysis",
                    record.beatmap_id
                );
            }
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        transaction.commit()?;
        self.ensure_indexes_invalidated()?;
        Ok(())
    }

    pub fn normalized_records(&self, version: u32) -> Result<Vec<ManiaFeatureRecord>> {
        Ok(self
            .normalized_records_with_offsets(version)?
            .into_iter()
            .map(|(_, record)| record)
            .collect())
    }

    pub fn normalized_records_with_offsets(
        &self,
        version: u32,
    ) -> Result<Vec<(u64, ManiaFeatureRecord)>> {
        let file_name = format!("mania-features-v{version}.bin");
        let mut statement = self.connection.prepare(
            "SELECT beatmap_id,normalized_offset FROM mania_analyses
             WHERE analyzer_version=?1 AND normalization_version=?2 AND status=2
             ORDER BY beatmap_id",
        )?;
        let locations = statement
            .query_map(
                params![MANIA_ANALYZER_VERSION as i64, version as i64],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let offsets = locations
            .iter()
            .map(|(_, offset)| *offset)
            .collect::<Vec<_>>();
        let records = self.normalized_records_at_offsets(version, &offsets)?;
        locations
            .into_iter()
            .zip(records)
            .map(|((beatmap_id, offset), record)| {
                if record.beatmap_id != beatmap_id {
                    bail!(
                        "mania normalized offset {offset} points to beatmap {} instead of {beatmap_id}",
                        record.beatmap_id
                    );
                }
                Ok((offset, record))
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("invalid mania normalized file {file_name}"))
    }

    pub fn normalized_records_at_offsets(
        &self,
        version: u32,
        offsets: &[u64],
    ) -> Result<Vec<ManiaFeatureRecord>> {
        let path = self.root.join(format!("mania-features-v{version}.bin"));
        let mut file = File::open(&path)
            .with_context(|| format!("missing mania feature file: {}", path.display()))?;
        let mut actual = [0_u8; 8];
        file.read_exact(&mut actual)?;
        if actual != *NORMALIZED_HEADER {
            bail!("invalid mania feature file header: {}", path.display());
        }
        offsets
            .iter()
            .map(|offset| {
                if *offset < NORMALIZED_HEADER.len() as u64 {
                    bail!("invalid mania normalized record offset {offset}");
                }
                file.seek(SeekFrom::Start(*offset))?;
                bincode::deserialize_from(&mut file)
                    .with_context(|| format!("invalid mania normalized record at offset {offset}"))
            })
            .collect()
    }

    pub fn normalized_for_id(&self, version: u32, beatmap_id: u64) -> Result<ManiaFeatureRecord> {
        let offset: i64 = self
            .connection
            .query_row(
                "SELECT normalized_offset FROM mania_analyses
             WHERE beatmap_id=?1 AND analyzer_version=?2 AND normalization_version=?3 AND status=2",
                params![
                    beatmap_id as i64,
                    MANIA_ANALYZER_VERSION as i64,
                    version as i64
                ],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .context("unknown normalized mania beatmap ID")?;
        self.read_record(
            &format!("mania-features-v{version}.bin"),
            NORMALIZED_HEADER,
            offset as u64,
        )
    }

    pub fn metadata_for(&self, beatmap_id: u64) -> Result<ManiaBeatmapMetadata> {
        self.connection
            .query_row(
                "SELECT beatmap_id,beatmapset_id,checksum,artist,title,version,creator,
                        online_url,key_count,mode_family,dominant_pattern
                 FROM mania_beatmaps WHERE beatmap_id=?1",
                [beatmap_id as i64],
                |row| {
                    Ok(ManiaBeatmapMetadata {
                        beatmap_id: row.get::<_, i64>(0)? as u64,
                        beatmapset_id: row.get::<_, i64>(1)? as u64,
                        checksum: row.get(2)?,
                        artist: row.get(3)?,
                        title: row.get(4)?,
                        version: row.get(5)?,
                        creator: row.get(6)?,
                        online_url: row.get(7)?,
                        key_count: row.get::<_, i64>(8)? as u8,
                        mode_family: mode_family_from_i64(row.get(9)?),
                        dominant_pattern: pattern_from_i64(row.get(10)?),
                    })
                },
            )
            .context("unknown mania beatmap metadata")
    }

    pub fn set_scan_counts(
        &self,
        eligible: usize,
        unsupported: usize,
        failed: usize,
    ) -> Result<()> {
        self.set_state("scan_eligible", &eligible.to_string())?;
        self.set_state("scan_unsupported", &unsupported.to_string())?;
        self.set_state("scan_failed", &failed.to_string())
    }

    pub fn scan_counts(&self) -> Result<(usize, usize, usize)> {
        Ok((
            self.state_usize("scan_eligible")?,
            self.state_usize("scan_unsupported")?,
            self.state_usize("scan_failed")?,
        ))
    }

    pub fn normalized_count(&self, version: u32) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM mania_analyses
             WHERE analyzer_version=?1 AND normalization_version=?2 AND status=2",
            params![MANIA_ANALYZER_VERSION as i64, version as i64],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM mania_state WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO mania_state(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn state_usize(&self, key: &str) -> Result<usize> {
        Ok(self
            .state(key)?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    fn offsets(&self, sql: &str, parameters: &[i64]) -> Result<Vec<u64>> {
        let mut statement = self.connection.prepare(sql)?;
        let values = statement.query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get::<_, i64>(0)
        })?;
        Ok(values
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|value| value as u64)
            .collect())
    }

    fn ensure_header(&self, file_name: &str, header: &[u8; 8]) -> Result<()> {
        let path = self.root.join(file_name);
        if !path.exists() {
            fs::write(path, header)?;
            return Ok(());
        }
        let mut actual = [0_u8; 8];
        File::open(&path)?.read_exact(&mut actual)?;
        if actual != *header {
            bail!("invalid mania feature file header: {}", path.display());
        }
        Ok(())
    }

    fn append_record<T: serde::Serialize>(
        &self,
        file_name: &str,
        header: &[u8; 8],
        record: &T,
    ) -> Result<u64> {
        self.ensure_header(file_name, header)?;
        let mut file = OpenOptions::new()
            .append(true)
            .read(true)
            .open(self.root.join(file_name))?;
        let offset = file.seek(SeekFrom::End(0))?;
        bincode::serialize_into(&mut file, record)?;
        file.flush()?;
        Ok(offset)
    }

    fn read_record<T: serde::de::DeserializeOwned>(
        &self,
        file_name: &str,
        header: &[u8; 8],
        offset: u64,
    ) -> Result<T> {
        let path = self.root.join(file_name);
        let mut file = File::open(&path)
            .with_context(|| format!("missing mania feature file: {}", path.display()))?;
        let mut actual = [0_u8; 8];
        file.read_exact(&mut actual)?;
        if actual != *header || offset < header.len() as u64 {
            bail!("invalid mania feature record source: {}", path.display());
        }
        file.seek(SeekFrom::Start(offset))?;
        bincode::deserialize_from(&mut file)
            .with_context(|| format!("invalid mania feature record at {offset}"))
    }

    fn invalidate_indexes(&self) -> Result<()> {
        let directory = self.root.join("indexes");
        if !directory.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if name.starts_with("mania-v")
                && (name.ends_with(".buckets") || name.ends_with(".buckets.sha256"))
            {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn ensure_indexes_invalidated(&mut self) -> Result<()> {
        if !self.indexes_invalidated {
            self.invalidate_indexes()?;
            self.indexes_invalidated = true;
        }
        Ok(())
    }
}

fn validate_raw(metadata: &ManiaBeatmapMetadata, record: &ManiaRawFeatureRecord) -> Result<()> {
    if metadata.beatmap_id != record.beatmap_id
        || metadata.beatmapset_id != record.beatmapset_id
        || metadata.key_count != record.key_count
        || metadata.mode_family != record.mode_family
        || metadata.dominant_pattern != record.dominant_pattern
    {
        bail!("mania metadata and feature record do not agree");
    }
    if record.analyzer_version != MANIA_ANALYZER_VERSION {
        bail!("mania raw feature has the wrong analyzer version");
    }
    if !matches!(record.key_count, 4 | 6 | 7) {
        bail!("mania raw feature has an unsupported key count");
    }
    if record
        .difficulty
        .as_array()
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
        || record
            .style
            .as_array()
            .into_iter()
            .any(|value| !(0.0..=1.0).contains(&value))
        || [
            record.base.bpm,
            record.base.length_seconds,
            record.base.active_length_seconds,
            record.base.note_count,
            record.base.row_count,
            record.base.avg_nps,
            record.base.peak_nps,
            record.base.break_density,
            record.base.sv_change_rate,
        ]
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
    {
        bail!("mania raw feature is non-finite or outside its declared range");
    }
    Ok(())
}

fn validate_normalized(record: &ManiaFeatureRecord, version: u32) -> Result<()> {
    if record.analyzer_version != MANIA_ANALYZER_VERSION
        || record.normalization_version != version
        || !matches!(record.key_count, 4 | 6 | 7)
        || record.difficulty_band > 9
        || !(0.0..=1.0).contains(&record.difficulty_percentile)
        || record
            .searchable_vector()
            .into_iter()
            .any(|value| !(0.0..=1.0).contains(&value))
    {
        bail!("invalid normalized mania record {}", record.beatmap_id);
    }
    Ok(())
}

fn mode_family_from_i64(value: i64) -> ManiaModeFamily {
    match value {
        1 => ManiaModeFamily::Hb,
        2 => ManiaModeFamily::Mix,
        3 => ManiaModeFamily::Ln,
        _ => ManiaModeFamily::Rc,
    }
}

fn pattern_from_i64(value: i64) -> ManiaPattern {
    match value {
        1 => ManiaPattern::Chordstream,
        2 => ManiaPattern::Jacks,
        3 => ManiaPattern::Coordination,
        4 => ManiaPattern::Density,
        5 => ManiaPattern::Wildcard,
        _ => ManiaPattern::Stream,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{ManiaDifficultyVector, ManiaStyleVector};

    fn sample(id: u64) -> (ManiaBeatmapMetadata, ManiaRawFeatureRecord) {
        let metadata = ManiaBeatmapMetadata {
            beatmap_id: id,
            beatmapset_id: id / 10,
            checksum: format!("checksum-{id}"),
            artist: "artist".into(),
            title: "title".into(),
            version: "version".into(),
            creator: "creator".into(),
            online_url: format!("https://osu.ppy.sh/b/{id}"),
            key_count: 4,
            mode_family: ManiaModeFamily::Rc,
            dominant_pattern: ManiaPattern::Stream,
        };
        let record = ManiaRawFeatureRecord {
            beatmap_id: id,
            beatmapset_id: id / 10,
            difficulty: ManiaDifficultyVector::from_array([id as f32; 8]),
            style: ManiaStyleVector::default(),
            key_count: 4,
            analyzer_version: MANIA_ANALYZER_VERSION,
            ..ManiaRawFeatureRecord::default()
        };
        (metadata, record)
    }

    #[test]
    fn raw_storage_is_resumable_and_separate_from_standard() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        let (metadata, record) = sample(10);
        assert!(store.append_raw(&metadata, &record)?);
        assert!(!store.append_raw(&metadata, &record)?);
        assert_eq!(store.raw_records()?, vec![record]);
        assert!(temp.path().join("mania-metadata.sqlite").exists());
        assert!(!temp.path().join("metadata.sqlite").exists());
        Ok(())
    }

    #[test]
    fn checksum_change_replaces_the_active_record() -> Result<()> {
        let temp = tempdir()?;
        let mut store = ManiaFeatureStore::open(temp.path())?;
        let (mut metadata, mut record) = sample(10);
        assert!(store.append_raw(&metadata, &record)?);
        metadata.checksum = "updated-checksum".into();
        record.difficulty.speed = 99.0;
        assert!(store.append_raw(&metadata, &record)?);
        assert_eq!(store.raw_records()?, vec![record]);
        assert!(store.current_analysis_matches(10, "updated-checksum")?);
        Ok(())
    }
}
