use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ManiaAnalyzer, ManiaFeatureRecord, ManiaFeatureStore, ManiaGameMod, ManiaNormalizer};

/// OPP's v1 variant sidecar format. The field order is part of the binary contract.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManiaModFeatureRecord {
    pub beatmap_id: u64,
    pub game_mod: ManiaGameMod,
    pub record: ManiaFeatureRecord,
}

/// Recompute DT/HT from the original source and apply the existing NM cohort
/// normalizer, matching OPP's local analyzer. Never scale normalized NM vectors.
pub fn export_mania_mod_features(root: &Path, version: u32) -> Result<usize> {
    let store = ManiaFeatureStore::open(root)?;
    let normalizer = ManiaNormalizer::load(root, version)?;
    let records = store.normalized_records(version)?;
    let temporary = root.join("mania-mod-features-v1.bin.tmp");
    let _ = fs::remove_file(&temporary);
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<usize> {
        output.write_all(b"ODLMMV1\0")?;
        let analyzer = ManiaAnalyzer::new();
        let mut count = 0;
        for record in records {
            let path = root
                .join("beatmaps")
                .join(format!("{}.osu", record.beatmap_id));
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| path.display().to_string()),
            };
            let metadata = store.metadata_for(record.beatmap_id)?;
            if hex::encode(Sha256::digest(&bytes)) != metadata.checksum {
                bail!(
                    "source checksum changed for {}; run mania-reanalyze first",
                    record.beatmap_id
                );
            }
            for game_mod in [ManiaGameMod::Dt, ManiaGameMod::Ht] {
                let (_, raw) = analyzer.analyze_bytes_with_beatmap_id_and_mod(
                    &bytes,
                    record.beatmap_id,
                    game_mod,
                )?;
                let entry = ManiaModFeatureRecord {
                    beatmap_id: record.beatmap_id,
                    game_mod,
                    record: normalizer.transform(&raw)?,
                };
                bincode::serialize_into(&mut output, &entry)?;
                count += 1;
            }
        }
        output.sync_all()?;
        Ok(count)
    })();
    drop(output);
    match result {
        Ok(count) => {
            let destination = root.join("mania-mod-features-v1.bin");
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(&temporary, &destination)?;
            Ok(count)
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}
