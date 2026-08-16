use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    ANALYZER_ALGORITHM_ID, ANALYZER_VERSION, BeatmapFeatureRecord, BeatmapMetadata,
    OVERLAP_ALGORITHM_VERSION, RAW_FEATURE_FILE, READING_ALGORITHM_VERSION, ROSU_PP_VERSION,
    RawFeatureRecord, StarSectionStats,
};

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
        fs::create_dir_all(root.join("beatmaps"))?;
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
            );
            CREATE TABLE IF NOT EXISTS analysis_versions (
              analyzer_version INTEGER PRIMARY KEY, algorithm_id TEXT NOT NULL, rosu_pp_version TEXT NOT NULL,
              reading_version TEXT NOT NULL, overlap_version TEXT NOT NULL, created_at INTEGER NOT NULL
            );")?;
        ensure_column(&connection, "beatmaps", "star_rating", "REAL")?;
        ensure_column(&connection, "beatmaps", "star_section", "INTEGER")?;
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_beatmaps_star_section ON beatmaps(star_section);
             CREATE TABLE IF NOT EXISTS star_section_stats (
               star_section INTEGER NOT NULL,
               analyzer_version INTEGER NOT NULL,
               normalization_version INTEGER NOT NULL,
               sample_count INTEGER NOT NULL,
               aim_sum REAL NOT NULL,
               speed_sum REAL NOT NULL,
               reading_sum REAL NOT NULL,
               slider_sum REAL NOT NULL,
               overlap_sum REAL NOT NULL,
               aim_sum_squares REAL NOT NULL,
               speed_sum_squares REAL NOT NULL,
               reading_sum_squares REAL NOT NULL,
               slider_sum_squares REAL NOT NULL,
               overlap_sum_squares REAL NOT NULL,
               ar_sum REAL NOT NULL,
               cs_sum REAL NOT NULL,
               od_sum REAL NOT NULL,
               ar_sum_squares REAL NOT NULL,
               cs_sum_squares REAL NOT NULL,
               od_sum_squares REAL NOT NULL,
               PRIMARY KEY (star_section, analyzer_version, normalization_version)
             );",
        )?;
        // Legacy star-section tables do not contain base-parameter statistics.
        // Nullable migration columns intentionally leave their existing rows
        // incomplete so validation requires a fresh normalizer publication.
        for column in [
            "ar_sum",
            "cs_sum",
            "od_sum",
            "ar_sum_squares",
            "cs_sum_squares",
            "od_sum_squares",
        ] {
            ensure_column(&connection, "star_section_stats", column, "REAL")?;
        }
        let registered: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM analysis_versions WHERE analyzer_version=?1)",
            [ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?;
        let existing_analyses: i64 = connection.query_row(
            "SELECT COUNT(*) FROM analyses WHERE analyzer_version=?1",
            [ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?;
        // A new/empty store can register the current snapshot immediately. If a
        // legacy database lost its version row, leave it unregistered so only an
        // explicit reanalysis can claim the existing Analyzer version.
        if !registered && existing_analyses == 0 {
            connection.execute(
                "INSERT INTO analysis_versions(analyzer_version,algorithm_id,rosu_pp_version,reading_version,overlap_version,created_at) VALUES(?1,?2,?3,?4,?5,unixepoch())",
                params![ANALYZER_VERSION as i64, ANALYZER_ALGORITHM_ID, ROSU_PP_VERSION, READING_ALGORITHM_VERSION, OVERLAP_ALGORITHM_VERSION],
            )?;
        }
        let store = Self { root, connection };
        store.ensure_header(RAW_FEATURE_FILE, b"ODLRAW1")?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the current Analyzer version was produced by the exact algorithm
    /// snapshot embedded in this executable and in OPP's read-only runtime.
    pub fn algorithm_snapshot_matches(&self) -> Result<bool> {
        let snapshot = self
            .connection
            .query_row(
                "SELECT algorithm_id,rosu_pp_version,reading_version,overlap_version FROM analysis_versions WHERE analyzer_version=?1",
                [ANALYZER_VERSION as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(snapshot.is_some_and(|snapshot| {
            snapshot.0 == ANALYZER_ALGORITHM_ID
                && snapshot.1 == ROSU_PP_VERSION
                && snapshot.2 == READING_ALGORITHM_VERSION
                && snapshot.3 == OVERLAP_ALGORITHM_VERSION
        }))
    }

    /// Prepare the current Analyzer version for a new formula or dependency snapshot.
    ///
    /// A same-version snapshot change invalidates that version's raw/normalized
    /// pointers. A newly registered Analyzer version preserves older versioned
    /// records but still removes published HNSW files before recalculation. The
    /// append-only raw file itself is intentionally retained.
    pub fn prepare_reanalysis(&mut self) -> Result<bool> {
        let snapshot_matches = self.algorithm_snapshot_matches()?;
        let current_analyses: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM analyses WHERE analyzer_version=?1",
            [ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?;
        let has_published_index = ["difficulty-main.hnsw", "difficulty-delta.hnsw"]
            .iter()
            .any(|name| self.root.join("indexes").join(name).exists());
        let needs_index_reset = current_analyses == 0 && has_published_index;
        if snapshot_matches && !needs_index_reset {
            return Ok(false);
        }

        if !snapshot_matches {
            let tx = self.connection.transaction()?;
            tx.execute(
                "DELETE FROM analyses WHERE analyzer_version=?1",
                [ANALYZER_VERSION as i64],
            )?;
            tx.execute(
                "DELETE FROM star_section_stats WHERE analyzer_version=?1",
                [ANALYZER_VERSION as i64],
            )?;
            tx.execute(
                "INSERT INTO analysis_versions(analyzer_version,algorithm_id,rosu_pp_version,reading_version,overlap_version,created_at) VALUES(?1,?2,?3,?4,?5,unixepoch()) ON CONFLICT(analyzer_version) DO UPDATE SET algorithm_id=excluded.algorithm_id,rosu_pp_version=excluded.rosu_pp_version,reading_version=excluded.reading_version,overlap_version=excluded.overlap_version,created_at=excluded.created_at",
                params![ANALYZER_VERSION as i64, ANALYZER_ALGORITHM_ID, ROSU_PP_VERSION, READING_ALGORITHM_VERSION, OVERLAP_ALGORITHM_VERSION],
            )?;
            tx.commit()?;
        }

        // Once SQLite advertises the new snapshot, no graph built from the old
        // vectors may remain discoverable. Missing indexes also make OPP fail
        // closed while reanalysis/normalization is still in progress.
        for name in [
            "difficulty-main.hnsw",
            "difficulty-main.hnsw.sha256",
            "difficulty-delta.hnsw",
            "difficulty-delta.hnsw.sha256",
        ] {
            let path = self.root.join("indexes").join(name);
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to invalidate {}", path.display()))?;
            }
        }
        Ok(true)
    }

    fn require_algorithm_snapshot(&self) -> Result<()> {
        if !self.algorithm_snapshot_matches()? {
            bail!(
                "Analyzer v{ANALYZER_VERSION} snapshot does not match {ROSU_PP_VERSION}; run reanalyze before generating or reading derived data"
            );
        }
        Ok(())
    }

    /// Fast resume check for `reanalyze`: retained sources are named by beatmap
    /// ID, so a matching source checksum and active Analyzer row can bypass the
    /// expensive difficulty calculation.
    pub fn current_analysis_matches(&self, beatmap_id: u64, checksum: &str) -> Result<bool> {
        self.require_algorithm_snapshot()?;
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM analyses INNER JOIN beatmaps USING(beatmap_id) WHERE analyses.beatmap_id=?1 AND analyses.mod_profile=0 AND analyses.analyzer_version=?2 AND analyses.status IN (1,2) AND beatmaps.checksum=?3)",
                params![beatmap_id as i64, ANALYZER_VERSION as i64, checksum],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Retain the original chart text.  Sources are intentionally flat and named by
    /// beatmap ID, which makes them stable across pack re-downloads and keeps this
    /// directory free of backgrounds, audio, videos, and other non-`.osu` assets.
    pub fn persist_beatmap_source(&self, beatmap_id: u64, bytes: &[u8]) -> Result<()> {
        fs::write(
            self.root.join("beatmaps").join(format!("{beatmap_id}.osu")),
            bytes,
        )?;
        Ok(())
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

    pub fn pack_is_complete(&self, id: &str) -> Result<bool> {
        let status: Option<String> = self
            .connection
            .query_row("SELECT status FROM packs WHERE pack_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(status.as_deref() == Some("complete"))
    }

    pub fn pack_is_excluded(&self, id: &str) -> Result<bool> {
        let status: Option<String> = self
            .connection
            .query_row("SELECT status FROM packs WHERE pack_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(status.as_deref() == Some("excluded"))
    }

    pub fn append_raw(
        &mut self,
        metadata: &BeatmapMetadata,
        record: &RawFeatureRecord,
    ) -> Result<bool> {
        self.require_algorithm_snapshot()?;
        if record.mod_profile != 0 {
            bail!("only NoMod (mod_profile=0) is supported");
        }
        let section = star_section(metadata.star_rating)?;
        let existing: Option<(String, Option<f64>, Option<i64>)> = self
            .connection
            .query_row(
                "SELECT beatmaps.checksum,beatmaps.star_rating,beatmaps.star_section FROM beatmaps INNER JOIN analyses \
                 ON analyses.beatmap_id=beatmaps.beatmap_id \
                 WHERE beatmaps.beatmap_id=?1 AND analyses.mod_profile=0 \
                   AND analyses.analyzer_version=?2",
                params![record.beatmap_id as i64, record.analyzer_version as i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|(checksum, rating, stored_section)| {
                checksum == &metadata.checksum
                    && rating.is_some_and(|value| value.is_finite())
                    && *stored_section == Some(section)
            })
        {
            return Ok(false);
        }
        if existing
            .as_ref()
            .is_some_and(|(checksum, _, _)| checksum == &metadata.checksum)
        {
            let tx = self.connection.transaction()?;
            tx.execute(
                "UPDATE beatmaps SET beatmapset_id=?1,checksum=?2,artist=?3,title=?4,version=?5,creator=?6,online_url=?7,star_rating=?8,star_section=?9,updated_at=unixepoch() WHERE beatmap_id=?10",
                params![metadata.beatmapset_id as i64,metadata.checksum,metadata.artist,metadata.title,metadata.version,metadata.creator,metadata.online_url,metadata.star_rating,section,metadata.beatmap_id as i64],
            )?;
            tx.execute("DELETE FROM star_section_stats", [])?;
            tx.commit()?;
            return Ok(false);
        }
        let offset = self.append_record(RAW_FEATURE_FILE, record)?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO beatmaps(beatmap_id,beatmapset_id,checksum,artist,title,version,creator,online_url,star_rating,star_section,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,unixepoch()) ON CONFLICT(beatmap_id) DO UPDATE SET beatmapset_id=excluded.beatmapset_id,checksum=excluded.checksum,artist=excluded.artist,title=excluded.title,version=excluded.version,creator=excluded.creator,online_url=excluded.online_url,star_rating=excluded.star_rating,star_section=excluded.star_section,updated_at=unixepoch()", params![metadata.beatmap_id as i64,metadata.beatmapset_id as i64,metadata.checksum,metadata.artist,metadata.title,metadata.version,metadata.creator,metadata.online_url,metadata.star_rating,section])?;
        tx.execute("INSERT INTO analyses(beatmap_id,mod_profile,analyzer_version,normalization_version,record_offset,status) VALUES(?1,0,?2,0,?3,1) ON CONFLICT(beatmap_id,mod_profile,analyzer_version) DO UPDATE SET normalization_version=0,record_offset=excluded.record_offset,normalized_offset=NULL,status=1", params![record.beatmap_id as i64,record.analyzer_version as i64,offset as i64])?;
        tx.execute("DELETE FROM star_section_stats", [])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn raw_records(&self) -> Result<Vec<RawFeatureRecord>> {
        self.require_algorithm_snapshot()?;
        let missing_stars: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM analyses INNER JOIN beatmaps USING(beatmap_id) WHERE analyses.analyzer_version=?1 AND analyses.mod_profile=0 AND analyses.status IN (1,2) AND (beatmaps.star_rating IS NULL OR beatmaps.star_section IS NULL)",
            [ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?;
        if missing_stars > 0 {
            bail!(
                "{missing_stars} active Analyzer v{ANALYZER_VERSION} records have no star rating; run reanalyze before fitting a normalizer"
            );
        }
        let mut statement = self.connection.prepare(
            "SELECT record_offset FROM analyses WHERE analyzer_version=?1 AND status IN (1,2) ORDER BY beatmap_id",
        )?;
        let offsets = statement
            .query_map([ANALYZER_VERSION as i64], |row| row.get::<_, i64>(0))?
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
        if records.is_empty() {
            bail!("cannot write an empty normalized dataset");
        }
        let stats = self.aggregate_star_section_stats(version, records)?;
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
        let backup_path = self.root.join(format!("{file}.bak"));
        if backup_path.exists() {
            fs::remove_file(&backup_path)?;
        }
        if final_path.exists() {
            fs::rename(&final_path, &backup_path)?;
        }
        if let Err(error) = fs::rename(self.root.join(&temporary), &final_path) {
            if backup_path.exists() {
                let _ = fs::rename(&backup_path, &final_path);
            }
            return Err(error.into());
        }
        let record_size = self.record_size::<BeatmapFeatureRecord>();
        let database_result = (|| -> Result<()> {
            let tx = self.connection.transaction()?;
            let analyzer_versions = records
                .iter()
                .map(|record| record.analyzer_version)
                .collect::<HashSet<_>>();
            for analyzer_version in analyzer_versions {
                tx.execute(
                    "UPDATE analyses SET normalization_version=0,normalized_offset=NULL,status=1 WHERE analyzer_version=?1 AND mod_profile=0 AND status=2",
                    [analyzer_version as i64],
                )?;
                tx.execute(
                    "DELETE FROM star_section_stats WHERE analyzer_version=?1",
                    [analyzer_version as i64],
                )?;
            }
            for (index, record) in records.iter().enumerate() {
                let offset = HEADER_LEN + index as u64 * record_size;
                let changed = tx.execute("UPDATE analyses SET normalization_version=?1,normalized_offset=?2,status=2 WHERE beatmap_id=?3 AND mod_profile=0 AND analyzer_version=?4 AND status IN (1,2)", params![version as i64,offset as i64,record.beatmap_id as i64,record.analyzer_version as i64])?;
                if changed != 1 {
                    bail!(
                        "normalized record {} has no matching valid analysis",
                        record.beatmap_id
                    );
                }
            }
            for stat in &stats {
                let sums = stat.sums;
                let squares = stat.sum_squares;
                tx.execute(
                    "INSERT INTO star_section_stats(star_section,analyzer_version,normalization_version,sample_count,aim_sum,speed_sum,reading_sum,slider_sum,overlap_sum,aim_sum_squares,speed_sum_squares,reading_sum_squares,slider_sum_squares,overlap_sum_squares,ar_sum,cs_sum,od_sum,ar_sum_squares,cs_sum_squares,od_sum_squares) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                    params![stat.star_section,stat.analyzer_version as i64,stat.normalization_version as i64,stat.sample_count as i64,sums[0],sums[1],sums[2],sums[3],sums[4],squares[0],squares[1],squares[2],squares[3],squares[4],stat.ar_sum,stat.cs_sum,stat.od_sum,stat.ar_sum_squares,stat.cs_sum_squares,stat.od_sum_squares],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = database_result {
            let _ = fs::remove_file(&final_path);
            if backup_path.exists() {
                let _ = fs::rename(&backup_path, &final_path);
            }
            return Err(error);
        }
        if backup_path.exists() {
            fs::remove_file(backup_path)?;
        }
        Ok(())
    }

    fn aggregate_star_section_stats(
        &self,
        version: u32,
        records: &[BeatmapFeatureRecord],
    ) -> Result<Vec<StarSectionStats>> {
        let mut seen = HashSet::with_capacity(records.len());
        let mut grouped: HashMap<(i64, u32, u32), StarSectionStats> = HashMap::new();
        for record in records {
            if record.mod_profile != 0 {
                bail!("only NoMod normalized records can be published");
            }
            if record.normalization_version != version {
                bail!(
                    "record {} declares normalization v{}, expected v{version}",
                    record.beatmap_id,
                    record.normalization_version
                );
            }
            if !seen.insert((
                record.beatmap_id,
                record.mod_profile,
                record.analyzer_version,
            )) {
                bail!(
                    "duplicate normalized record for beatmap {}",
                    record.beatmap_id
                );
            }
            let values = record.difficulty.as_array();
            if values.iter().any(|value| !value.is_finite()) {
                bail!(
                    "record {} has a non-finite difficulty value",
                    record.beatmap_id
                );
            }
            let base_values = [record.base.ar, record.base.cs, record.base.od];
            if base_values.iter().any(|value| !value.is_finite()) {
                bail!(
                    "record {} has a non-finite AR, CS, or OD value",
                    record.beatmap_id
                );
            }
            let (rating, section): (f64, i64) = self.connection.query_row(
                "SELECT beatmaps.star_rating,beatmaps.star_section FROM beatmaps INNER JOIN analyses USING(beatmap_id) WHERE beatmaps.beatmap_id=?1 AND analyses.mod_profile=0 AND analyses.analyzer_version=?2 AND analyses.status IN (1,2) AND beatmaps.star_rating IS NOT NULL AND beatmaps.star_section IS NOT NULL",
                params![record.beatmap_id as i64, record.analyzer_version as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).with_context(|| format!("normalized record {} is missing valid star metadata or analysis state", record.beatmap_id))?;
            if star_section(rating)? != section {
                bail!(
                    "beatmap {} has inconsistent star_rating/star_section metadata",
                    record.beatmap_id
                );
            }
            let stat = grouped
                .entry((section, record.analyzer_version, version))
                .or_insert_with(|| StarSectionStats {
                    star_section: section,
                    analyzer_version: record.analyzer_version,
                    normalization_version: version,
                    ..StarSectionStats::default()
                });
            stat.sample_count += 1;
            let mut sums = stat.sums;
            let mut squares = stat.sum_squares;
            for index in 0..5 {
                let value = values[index] as f64;
                sums[index] += value;
                squares[index] += value * value;
            }
            stat.sums = sums;
            stat.sum_squares = squares;
            let [ar, cs, od] = base_values.map(f64::from);
            stat.ar_sum += ar;
            stat.cs_sum += cs;
            stat.od_sum += od;
            stat.ar_sum_squares += ar * ar;
            stat.cs_sum_squares += cs * cs;
            stat.od_sum_squares += od * od;
        }
        let mut stats = grouped.into_values().collect::<Vec<_>>();
        stats.sort_by_key(|stat| {
            (
                stat.star_section,
                stat.analyzer_version,
                stat.normalization_version,
            )
        });
        Ok(stats)
    }

    pub fn star_section_stats(&self, version: u32) -> Result<Vec<StarSectionStats>> {
        let mut statement = self.connection.prepare(
            "SELECT star_section,analyzer_version,normalization_version,sample_count,aim_sum,speed_sum,reading_sum,slider_sum,overlap_sum,aim_sum_squares,speed_sum_squares,reading_sum_squares,slider_sum_squares,overlap_sum_squares,ar_sum,cs_sum,od_sum,ar_sum_squares,cs_sum_squares,od_sum_squares FROM star_section_stats WHERE normalization_version=?1 ORDER BY star_section,analyzer_version",
        )?;
        statement
            .query_map([version as i64], |row| {
                Ok(StarSectionStats {
                    star_section: row.get(0)?,
                    analyzer_version: row.get::<_, i64>(1)? as u32,
                    normalization_version: row.get::<_, i64>(2)? as u32,
                    sample_count: row.get::<_, i64>(3)? as u64,
                    sums: [
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ],
                    sum_squares: [
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                    ],
                    ar_sum: row.get(14)?,
                    cs_sum: row.get(15)?,
                    od_sum: row.get(16)?,
                    ar_sum_squares: row.get(17)?,
                    cs_sum_squares: row.get(18)?,
                    od_sum_squares: row.get(19)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn validate_star_section_stats(&self, version: u32) -> Result<()> {
        let records = self.normalized_records(version)?;
        let expected = self.aggregate_star_section_stats(version, &records)?;
        let actual = self.star_section_stats(version)?;
        if actual != expected {
            bail!("star-section statistics do not match active normalized records");
        }
        Ok(())
    }

    pub fn normalized_records(&self, version: u32) -> Result<Vec<BeatmapFeatureRecord>> {
        self.require_algorithm_snapshot()?;
        let file = format!("features-v{version}.bin");
        let mut statement = self.connection.prepare("SELECT normalized_offset FROM analyses WHERE analyzer_version=?1 AND normalization_version=?2 AND status=2 ORDER BY beatmap_id")?;
        let offsets = statement
            .query_map(params![ANALYZER_VERSION as i64, version as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let records = offsets
            .into_iter()
            .map(|offset| self.read_record::<BeatmapFeatureRecord>(&file, offset as u64))
            .collect::<Result<Vec<_>>>()?;
        for record in &records {
            if record.analyzer_version != ANALYZER_VERSION
                || record.normalization_version != version
                || record.mod_profile != 0
            {
                bail!(
                    "normalized feature record {} has version/profile metadata inconsistent with SQLite",
                    record.beatmap_id
                );
            }
        }
        Ok(records)
    }

    pub fn normalized_for_id(&self, version: u32, beatmap_id: u64) -> Result<BeatmapFeatureRecord> {
        let file = format!("features-v{version}.bin");
        let offset: i64 = self.connection.query_row("SELECT normalized_offset FROM analyses WHERE beatmap_id=?1 AND normalization_version=?2 AND status=2 ORDER BY analyzer_version DESC LIMIT 1", params![beatmap_id as i64,version as i64], |row| row.get::<_, Option<i64>>(0))?.context("unknown normalized beatmap ID")?;
        self.read_record(&file, offset as u64)
    }

    pub fn metadata_for(&self, beatmap_id: u64) -> Result<BeatmapMetadata> {
        self.connection.query_row("SELECT beatmap_id,beatmapset_id,checksum,artist,title,version,creator,online_url,star_rating FROM beatmaps WHERE beatmap_id=?1 AND star_rating IS NOT NULL", [beatmap_id as i64], |row| Ok(BeatmapMetadata { beatmap_id: row.get::<_,i64>(0)? as u64, beatmapset_id: row.get::<_,i64>(1)? as u64, checksum: row.get(2)?, artist: row.get(3)?, title: row.get(4)?, version: row.get(5)?, creator: row.get(6)?, online_url: row.get(7)?, star_rating: row.get(8)? })) .context("unknown beatmap metadata or missing star rating")
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

pub fn star_section(star_rating: f64) -> Result<i64> {
    if !star_rating.is_finite() || star_rating < 0.0 {
        bail!("star rating must be a finite non-negative value");
    }
    Ok((star_rating / 0.1 + 1e-6).floor() as i64)
}

fn ensure_column(connection: &Connection, table: &str, column: &str, sql_type: &str) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |row| row.get(0),
    )?;
    if !exists {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {sql_type}"
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DifficultyVector;
    use tempfile::tempdir;

    fn metadata(id: u64, stars: f64) -> BeatmapMetadata {
        BeatmapMetadata {
            beatmap_id: id,
            beatmapset_id: id / 10,
            checksum: format!("checksum-{id}"),
            artist: "artist".into(),
            title: "title".into(),
            version: "version".into(),
            creator: "creator".into(),
            online_url: format!("https://osu.ppy.sh/b/{id}"),
            star_rating: stars,
        }
    }

    fn raw(id: u64, analyzer_version: u32) -> RawFeatureRecord {
        RawFeatureRecord {
            beatmap_id: id,
            beatmapset_id: id / 10,
            analyzer_version,
            mod_profile: 0,
            ..RawFeatureRecord::default()
        }
    }

    fn normalized(
        id: u64,
        analyzer_version: u32,
        normalization_version: u32,
        values: [f32; 5],
    ) -> BeatmapFeatureRecord {
        normalized_with_base(
            id,
            analyzer_version,
            normalization_version,
            values,
            [0.0; 3],
        )
    }

    fn normalized_with_base(
        id: u64,
        analyzer_version: u32,
        normalization_version: u32,
        values: [f32; 5],
        base_values: [f32; 3],
    ) -> BeatmapFeatureRecord {
        let [ar, cs, od] = base_values;
        BeatmapFeatureRecord {
            beatmap_id: id,
            beatmapset_id: id / 10,
            difficulty: DifficultyVector::from_array(values),
            base: crate::BaseFeatures {
                ar,
                cs,
                od,
                ..crate::BaseFeatures::default()
            },
            analyzer_version,
            normalization_version,
            mod_profile: 0,
            ..BeatmapFeatureRecord::default()
        }
    }

    #[test]
    fn star_sections_handle_decimal_boundaries() -> Result<()> {
        assert_eq!(star_section(5.7)?, 57);
        assert_eq!(star_section(6.1)?, 61);
        assert_eq!(star_section(6.59999)?, 65);
        Ok(())
    }

    #[test]
    fn schema_migration_invalidates_legacy_stats_until_rebuilt() -> Result<()> {
        let temp = tempdir()?;
        {
            let legacy = Connection::open(temp.path().join("metadata.sqlite"))?;
            legacy.execute_batch(
                "CREATE TABLE beatmaps (
                   beatmap_id INTEGER PRIMARY KEY, beatmapset_id INTEGER NOT NULL,
                   checksum TEXT NOT NULL, artist TEXT NOT NULL, title TEXT NOT NULL,
                   version TEXT NOT NULL, creator TEXT NOT NULL, online_url TEXT NOT NULL,
                   updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE star_section_stats (
                   star_section INTEGER NOT NULL, analyzer_version INTEGER NOT NULL,
                   normalization_version INTEGER NOT NULL, sample_count INTEGER NOT NULL,
                   aim_sum REAL NOT NULL, speed_sum REAL NOT NULL, reading_sum REAL NOT NULL,
                   slider_sum REAL NOT NULL, overlap_sum REAL NOT NULL,
                   aim_sum_squares REAL NOT NULL, speed_sum_squares REAL NOT NULL,
                   reading_sum_squares REAL NOT NULL, slider_sum_squares REAL NOT NULL,
                   overlap_sum_squares REAL NOT NULL,
                   PRIMARY KEY (star_section, analyzer_version, normalization_version)
                 );
                 INSERT INTO star_section_stats VALUES(
                   42, 3, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
                 );",
            )?;
        }
        let mut store = FeatureStore::open(temp.path())?;
        let connection = Connection::open(temp.path().join("metadata.sqlite"))?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_beatmaps_star_section')",
            [],
            |row| row.get(0),
        )?;
        assert!(exists);
        let columns = connection
            .prepare("SELECT name FROM pragma_table_info('beatmaps')")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        assert!(columns.contains("star_rating"));
        assert!(columns.contains("star_section"));
        drop(connection);

        assert!(store.star_section_stats(1).is_err());
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        store.write_normalized(
            1,
            &[normalized_with_base(
                10,
                ANALYZER_VERSION,
                1,
                [0.25; 5],
                [8.0, 4.0, 7.0],
            )],
        )?;
        store.validate_star_section_stats(1)?;
        Ok(())
    }

    #[test]
    fn normalized_binary_record_layout_remains_compatible() -> Result<()> {
        assert_eq!(
            bincode::serialized_size(&BeatmapFeatureRecord::default())?,
            124
        );
        Ok(())
    }

    #[test]
    fn statistics_are_exact_idempotent_and_version_separated() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 5.71), &raw(10, ANALYZER_VERSION))?;
        store.append_raw(&metadata(20, 5.79), &raw(20, ANALYZER_VERSION))?;
        store.append_raw(&metadata(30, 5.75), &raw(30, ANALYZER_VERSION + 1))?;
        let first = normalized_with_base(
            10,
            ANALYZER_VERSION,
            7,
            [0.1, 0.2, 0.3, 0.4, 0.5],
            [8.0, 4.0, 7.0],
        );
        let second = normalized_with_base(
            20,
            ANALYZER_VERSION,
            7,
            [0.2, 0.3, 0.4, 0.5, 0.6],
            [9.0, 4.5, 8.0],
        );
        let other_analyzer =
            normalized_with_base(30, ANALYZER_VERSION + 1, 7, [0.9; 5], [9.5, 5.0, 9.0]);

        let grouped = store.aggregate_star_section_stats(7, &[first, second, other_analyzer])?;
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].analyzer_version, ANALYZER_VERSION);
        assert_eq!(grouped[0].sample_count, 2);
        let expected_sums = [0.3, 0.5, 0.7, 0.9, 1.1];
        let expected_squares = [0.05, 0.13, 0.25, 0.41, 0.61];
        for index in 0..5 {
            assert!((grouped[0].sums[index] - expected_sums[index]).abs() < 1e-6);
            assert!((grouped[0].sum_squares[index] - expected_squares[index]).abs() < 1e-6);
        }
        assert_eq!(grouped[0].ar_sum, 17.0);
        assert_eq!(grouped[0].cs_sum, 8.5);
        assert_eq!(grouped[0].od_sum, 15.0);
        assert_eq!(grouped[0].ar_sum_squares, 145.0);
        assert_eq!(grouped[0].cs_sum_squares, 36.25);
        assert_eq!(grouped[0].od_sum_squares, 113.0);
        assert_eq!(grouped[1].analyzer_version, ANALYZER_VERSION + 1);
        assert_eq!(grouped[1].sample_count, 1);

        store.write_normalized(7, &[first, second])?;
        let once = store.star_section_stats(7)?;
        store.write_normalized(7, &[first, second])?;
        assert_eq!(store.star_section_stats(7)?, once);
        store.validate_star_section_stats(7)?;
        Ok(())
    }

    #[test]
    fn invalid_records_do_not_replace_existing_statistics() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        let valid = normalized(10, ANALYZER_VERSION, 1, [0.25; 5]);
        store.write_normalized(1, &[valid])?;
        let stats = store.star_section_stats(1)?;
        let file = fs::read(temp.path().join("features-v1.bin"))?;

        let invalid = normalized(999, ANALYZER_VERSION, 1, [0.5; 5]);
        assert!(store.write_normalized(1, &[valid, invalid]).is_err());
        assert_eq!(store.star_section_stats(1)?, stats);
        assert_eq!(fs::read(temp.path().join("features-v1.bin"))?, file);
        Ok(())
    }

    #[test]
    fn non_finite_base_parameters_do_not_replace_existing_statistics() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        let valid = normalized_with_base(10, ANALYZER_VERSION, 1, [0.25; 5], [8.0, 4.0, 7.0]);
        store.write_normalized(1, &[valid])?;
        let stats = store.star_section_stats(1)?;
        let file = fs::read(temp.path().join("features-v1.bin"))?;

        let invalid =
            normalized_with_base(10, ANALYZER_VERSION, 1, [0.25; 5], [f32::NAN, 4.0, 7.0]);
        assert!(store.write_normalized(1, &[invalid]).is_err());
        assert_eq!(store.star_section_stats(1)?, stats);
        assert_eq!(fs::read(temp.path().join("features-v1.bin"))?, file);
        Ok(())
    }

    #[test]
    fn new_raw_data_invalidates_published_statistics() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        store.write_normalized(1, &[normalized(10, ANALYZER_VERSION, 1, [0.25; 5])])?;
        assert!(!store.star_section_stats(1)?.is_empty());

        store.append_raw(&metadata(20, 4.3), &raw(20, ANALYZER_VERSION))?;
        assert!(store.star_section_stats(1)?.is_empty());
        assert!(store.validate_star_section_stats(1).is_err());
        Ok(())
    }

    #[test]
    fn missing_legacy_stars_fail_dataset_generation() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        store.connection.execute(
            "UPDATE beatmaps SET star_rating=NULL,star_section=NULL WHERE beatmap_id=10",
            [],
        )?;
        let error = store.raw_records().unwrap_err().to_string();
        assert!(error.contains("run reanalyze"));
        Ok(())
    }

    #[test]
    fn dependency_snapshot_change_requires_explicit_reanalysis() -> Result<()> {
        let temp = tempdir()?;
        let mut store = FeatureStore::open(temp.path())?;
        store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION))?;
        for name in [
            "difficulty-main.hnsw",
            "difficulty-main.hnsw.sha256",
            "difficulty-delta.hnsw",
            "difficulty-delta.hnsw.sha256",
        ] {
            fs::write(temp.path().join("indexes").join(name), b"stale")?;
        }
        store.connection.execute(
            "UPDATE analysis_versions SET rosu_pp_version='4.0.1' WHERE analyzer_version=?1",
            [ANALYZER_VERSION as i64],
        )?;

        assert!(!store.algorithm_snapshot_matches()?);
        assert!(
            store
                .raw_records()
                .unwrap_err()
                .to_string()
                .contains("run reanalyze")
        );
        assert!(store.prepare_reanalysis()?);
        assert!(store.algorithm_snapshot_matches()?);
        let analyses: i64 = store.connection.query_row(
            "SELECT COUNT(*) FROM analyses WHERE analyzer_version=?1",
            [ANALYZER_VERSION as i64],
            |row| row.get(0),
        )?;
        assert_eq!(analyses, 0);
        for name in [
            "difficulty-main.hnsw",
            "difficulty-main.hnsw.sha256",
            "difficulty-delta.hnsw",
            "difficulty-delta.hnsw.sha256",
        ] {
            assert!(!temp.path().join("indexes").join(name).exists());
        }
        assert!(!store.prepare_reanalysis()?);
        assert!(store.append_raw(&metadata(10, 4.2), &raw(10, ANALYZER_VERSION),)?);
        Ok(())
    }
}
