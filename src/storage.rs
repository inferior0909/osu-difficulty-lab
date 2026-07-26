use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{BeatmapFeatureRecord, BeatmapMetadata, RAW_FEATURE_FILE, RawFeatureRecord};

const HEADER_LEN: u64 = 32;
const FORMAT_VERSION: u32 = 1;

pub struct FeatureStore {
    root: PathBuf,
    connection: Connection,
}

impl FeatureStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("normalizers"))?;
        fs::create_dir_all(root.join("indexes"))?;
        fs::create_dir_all(root.join("tmp"))?;
        let connection = Connection::open(root.join("metadata.sqlite"))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS beatmaps (
              beatmap_id INTEGER PRIMARY KEY, beatmapset_id INTEGER NOT NULL, checksum TEXT NOT NULL,
              artist TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, creator TEXT NOT NULL,
              online_url TEXT NOT NULL, updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS analyses (
              beatmap_id INTEGER NOT NULL, mod_profile INTEGER NOT NULL, analyzer_version INTEGER NOT NULL,
              normalization_version INTEGER NOT NULL DEFAULT 0, record_offset INTEGER NOT NULL,
              normalized_offset INTEGER, status INTEGER NOT NULL, PRIMARY KEY (beatmap_id, mod_profile, analyzer_version)
            );
            CREATE TABLE IF NOT EXISTS packs (
              pack_id TEXT PRIMARY KEY, source_url TEXT NOT NULL, status TEXT NOT NULL, last_error TEXT, updated_at INTEGER NOT NULL
            );")?;
        let store = Self { root, connection };
        store.ensure_header(RAW_FEATURE_FILE, b"ODLRAW1")?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mark_pack(
        &self,
        id: &str,
        source_url: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.connection.execute("INSERT INTO packs(pack_id,source_url,status,last_error,updated_at) VALUES(?1,?2,?3,?4,unixepoch()) ON CONFLICT(pack_id) DO UPDATE SET status=excluded.status,last_error=excluded.last_error,updated_at=unixepoch()", params![id,source_url,status,error])?;
        Ok(())
    }

    pub fn append_raw(
        &mut self,
        metadata: &BeatmapMetadata,
        record: &RawFeatureRecord,
    ) -> Result<bool> {
        if record.mod_profile != 0 {
            bail!("only NoMod (mod_profile=0) is supported");
        }
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT checksum FROM beatmaps WHERE beatmap_id=?1",
                [record.beatmap_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        if existing.as_deref() == Some(metadata.checksum.as_str()) {
            return Ok(false);
        }
        let offset = self.append_record(RAW_FEATURE_FILE, record)?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO beatmaps(beatmap_id,beatmapset_id,checksum,artist,title,version,creator,online_url,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,unixepoch()) ON CONFLICT(beatmap_id) DO UPDATE SET beatmapset_id=excluded.beatmapset_id,checksum=excluded.checksum,artist=excluded.artist,title=excluded.title,version=excluded.version,creator=excluded.creator,online_url=excluded.online_url,updated_at=unixepoch()", params![metadata.beatmap_id as i64,metadata.beatmapset_id as i64,metadata.checksum,metadata.artist,metadata.title,metadata.version,metadata.creator,metadata.online_url])?;
        tx.execute("INSERT INTO analyses(beatmap_id,mod_profile,analyzer_version,normalization_version,record_offset,status) VALUES(?1,0,?2,0,?3,1) ON CONFLICT(beatmap_id,mod_profile,analyzer_version) DO UPDATE SET normalization_version=0,record_offset=excluded.record_offset,normalized_offset=NULL,status=1", params![record.beatmap_id as i64,record.analyzer_version as i64,offset as i64])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn raw_records(&self) -> Result<Vec<RawFeatureRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT record_offset FROM analyses WHERE status IN (1,2) ORDER BY beatmap_id",
        )?;
        let offsets = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        offsets
            .into_iter()
            .map(|offset| self.read_record(RAW_FEATURE_FILE, offset as u64))
            .collect()
    }

    pub fn raw_for_id(&self, beatmap_id: u64) -> Result<RawFeatureRecord> {
        let offset: i64 = self.connection.query_row("SELECT record_offset FROM analyses WHERE beatmap_id=?1 AND mod_profile=0 ORDER BY analyzer_version DESC LIMIT 1", [beatmap_id as i64], |row| row.get(0)).context("unknown beatmap ID")?;
        self.read_record(RAW_FEATURE_FILE, offset as u64)
    }

    pub fn write_normalized(
        &mut self,
        version: u32,
        records: &[BeatmapFeatureRecord],
    ) -> Result<()> {
        let file = format!("features-v{version}.bin");
        let temporary = format!("{file}.tmp");
        let temporary_path = self.root.join(&temporary);
        if temporary_path.exists() {
            fs::remove_file(&temporary_path)?;
        }
        self.ensure_header(&temporary, b"ODLNORM")?;
        for record in records {
            self.append_record(&temporary, record)?;
        }
        let final_path = self.root.join(&file);
        if final_path.exists() {
            fs::remove_file(&final_path)?;
        }
        fs::rename(self.root.join(&temporary), final_path)?;
        let record_size = self.record_size::<BeatmapFeatureRecord>();
        let tx = self.connection.transaction()?;
        for (index, record) in records.iter().enumerate() {
            let offset = HEADER_LEN + index as u64 * record_size;
            tx.execute("UPDATE analyses SET normalization_version=?1,normalized_offset=?2,status=2 WHERE beatmap_id=?3 AND mod_profile=0 AND analyzer_version=?4", params![version as i64,offset as i64,record.beatmap_id as i64,record.analyzer_version as i64])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn normalized_records(&self, version: u32) -> Result<Vec<BeatmapFeatureRecord>> {
        let file = format!("features-v{version}.bin");
        let mut statement = self.connection.prepare("SELECT normalized_offset FROM analyses WHERE normalization_version=?1 AND status=2 ORDER BY beatmap_id")?;
        let offsets = statement
            .query_map([version as i64], |row| row.get::<_, Option<i64>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        offsets
            .into_iter()
            .flatten()
            .map(|offset| self.read_record(&file, offset as u64))
            .collect()
    }

    pub fn normalized_for_id(&self, version: u32, beatmap_id: u64) -> Result<BeatmapFeatureRecord> {
        let file = format!("features-v{version}.bin");
        let offset: i64 = self.connection.query_row("SELECT normalized_offset FROM analyses WHERE beatmap_id=?1 AND normalization_version=?2 AND status=2 ORDER BY analyzer_version DESC LIMIT 1", params![beatmap_id as i64,version as i64], |row| row.get::<_, Option<i64>>(0))?.context("unknown normalized beatmap ID")?;
        self.read_record(&file, offset as u64)
    }

    pub fn metadata_for(&self, beatmap_id: u64) -> Result<BeatmapMetadata> {
        self.connection.query_row("SELECT beatmap_id,beatmapset_id,checksum,artist,title,version,creator,online_url FROM beatmaps WHERE beatmap_id=?1", [beatmap_id as i64], |row| Ok(BeatmapMetadata { beatmap_id: row.get::<_,i64>(0)? as u64, beatmapset_id: row.get::<_,i64>(1)? as u64, checksum: row.get(2)?, artist: row.get(3)?, title: row.get(4)?, version: row.get(5)?, creator: row.get(6)?, online_url: row.get(7)? })) .context("unknown beatmap metadata")
    }

    fn ensure_header(&self, name: &str, magic: &[u8; 7]) -> Result<()> {
        let path = self.root.join(name);
        if path.exists() {
            return Ok(());
        }
        let mut file = File::create(path)?;
        let mut header = [0_u8; HEADER_LEN as usize];
        header[..7].copy_from_slice(magic);
        header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        file.write_all(&header)?;
        file.sync_all()?;
        Ok(())
    }
    fn record_size<T: serde::Serialize + Default>(&self) -> u64 {
        bincode::serialized_size(&T::default()).expect("fixed record size")
    }
    fn append_record<T: serde::Serialize + Default>(&self, name: &str, record: &T) -> Result<u64> {
        let path = self.root.join(name);
        let mut file = OpenOptions::new().append(true).read(true).open(&path)?;
        let offset = file.seek(SeekFrom::End(0))?;
        let bytes = bincode::serialize(record)?;
        if bytes.len() as u64 != self.record_size::<T>() {
            bail!("feature record has a variable encoded size");
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(offset)
    }
    fn read_record<T: serde::de::DeserializeOwned + serde::Serialize + Default>(
        &self,
        name: &str,
        offset: u64,
    ) -> Result<T> {
        let mut file = File::open(self.root.join(name))?;
        let size = self.record_size::<T>() as usize;
        let mut bytes = vec![0; size];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        Ok(bincode::deserialize(&bytes)?)
    }
}
