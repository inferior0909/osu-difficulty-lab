# osu-difficulty-lab

An independent Rust research library and CLI for creating a reproducible five-dimensional `osu!standard` difficulty dataset and local similarity index. It does not depend on OPP.

## Current scope

- NoMod `osu!standard` only.
- Aim, Speed, and Flashlight use pinned `rosu-pp` difficulty attributes.
- Reading is a versioned 400 ms density × AR-pressure baseline.
- Overlap measures visible spatial interference, stacks, slider-path proximity, order ambiguity, and movement crossings.
- Raw records, normalized records, SQLite metadata, a persisted HNSW main index, and a delta-index placeholder remain local. Source beatmap archives are transient.

## Workflow

```powershell
cargo run -- init .\data
cargo run -- ingest-local .\data .\fixture-pack.zip
cargo run -- normalizer-fit .\data --version 1
cargo run -- index-build .\data --version 1
cargo run -- query .\data 12345 --version 1
```

`catalog-sync` writes the official pack IDs from all catalogue categories. `ingest-packs` requires a user-exported Netscape cookie file for `osu.ppy.sh`; the cookie is read only for the request and is never persisted by the application. Failed imports retain their single temporary archive for retry; successful imports delete it only after records are committed.

Training export is available as `export-parquet` (and a lightweight `export-csv`). The normalized binary feature store is the canonical query data; generated data directories, archives, cookie files, and exports are ignored by Git.

## Validation

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

This project is not affiliated with ppy Pty Ltd. `osu!` is a trademark of ppy Pty Ltd.
