# osu-difficulty-lab

An independent Rust research library and CLI for creating a reproducible five-dimensional `osu!standard` difficulty dataset and local similarity index. It does not depend on OPP.

## Current scope

- NoMod `osu!standard` only.
- Aim, Speed, and Flashlight use pinned `rosu-pp` difficulty attributes.
- Reading is a versioned 400 ms density × AR-pressure baseline.
- Overlap measures visible spatial interference, stacks, slider-path proximity, order ambiguity, and movement crossings.
- Raw records, normalized records, SQLite metadata, a persisted HNSW main index, and a delta-index placeholder remain local. Source beatmap archives are transient.

## Analysis version

Every `metadata.sqlite` database registers the algorithm snapshot used by its records. The current snapshot is `analyzer_version = 2` / `five-dimension-baseline-v2`:

- `rosu-pp 4.0.1`: NoMod Aim, Speed, and Flashlight attributes.
- `reading-density-ar-section-v1`: 400 ms non-spinner density sections, effective AR pressure, descending peak aggregation with a `0.90` decay.
- `overlap-visibility-spatial-strain-v1`: AR visibility window, 3 s candidate window, spatial/near overlap, stack pressure, slider-path proximity, movement crossing, and 400 ms strain peaks.

Changing any formula, dependency snapshot, or default weight requires a new `ANALYZER_VERSION`; records are then appended rather than overwritten.

## Workflow

```powershell
cargo run -- init .\data
cargo run -- ingest-local .\data .\fixture-pack.zip
cargo run -- normalizer-fit .\data --version 1
cargo run -- index-build .\data --version 1
cargo run -- query .\data 12345 --version 1
```

`catalog-sync` writes the official pack IDs from all catalogue categories. `ingest-packs` requires a user-exported Netscape cookie file for `osu.ppy.sh`; the cookie is read only for the request and is never persisted by the application. Failed imports retain their single temporary archive for retry; successful imports delete it only after records are committed.

## Official Pack batch script

Export your logged-in `osu.ppy.sh` cookies in Netscape format, keep that file outside the repository, then run:

```powershell
.\scripts\import-official-packs.ps1 -CookieFile C:\secure\osu-cookies.txt -Release
```

The script retrieves the official Pack catalogue, opens each official Pack page with the supplied session to obtain its signed download URL, then processes packs sequentially. It preserves only `data\official-pack-ids.txt`, the analysis/index database, and `data\failed-pack-ids.txt` when needed. Each successfully committed archive is deleted before the next Pack begins; already-complete Pack IDs are skipped on a later run. Use `-RefreshCatalog` to fetch the list again, or `-BatchSize 1` (the default) for the lowest temporary disk use.

Training export is available as `export-parquet` (and a lightweight `export-csv`). The normalized binary feature store is the canonical query data; generated data directories, archives, cookie files, and exports are ignored by Git.

## Validation

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

This project is not affiliated with ppy Pty Ltd. `osu!` is a trademark of ppy Pty Ltd.
