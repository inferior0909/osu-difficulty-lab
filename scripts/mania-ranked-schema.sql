-- Separate catalogue for the raw osu!mania Ranked corpus.
-- Run once: sqlite3 E:\\osu-mania-ranked\\mania-ranked.sqlite ".read scripts/mania-ranked-schema.sql"

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS mania_ranked_beatmaps (
    beatmap_id       INTEGER PRIMARY KEY,
    beatmapset_id    INTEGER NOT NULL,
    checksum         TEXT,
    artist           TEXT NOT NULL,
    title            TEXT NOT NULL,
    version          TEXT NOT NULL,
    creator          TEXT NOT NULL,
    ranked_date      TEXT,
    last_updated     TEXT,
    status           TEXT NOT NULL CHECK (status = 'ranked'),
    mode             TEXT NOT NULL CHECK (mode = 'mania'),
    downloaded_path  TEXT,
    downloaded_at    TEXT,
    download_status  TEXT NOT NULL DEFAULT 'pending'
                     CHECK (download_status IN ('pending', 'downloaded', 'failed')),
    last_error       TEXT,
    catalogued_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mania_ranked_beatmaps_set_id
    ON mania_ranked_beatmaps (beatmapset_id);

CREATE INDEX IF NOT EXISTS idx_mania_ranked_beatmaps_download_status
    ON mania_ranked_beatmaps (download_status);

