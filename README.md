# osu-difficulty-lab

`osu-difficulty-lab` 是一个用于分析 `osu!standard` 谱面的 Rust 工具。

它会把谱面转换成五个难度特征：瞄准、速度、读图、滑条和物件重叠，并在本地建立相似谱面索引。你可以用它查找「玩法和手感接近」的谱面，也可以导出数据做研究。

当前版本：`v0.1.0`

## 支持范围

- 只支持 `osu!standard` 和 NoMod。
- 支持导入本地谱包，以及从官方谱包目录下载谱面。
- 官方批量下载会保留纯 `osu!standard` 包和所有可能混合的包，因此不会漏掉含 standard 谱面的混合包；纯 taiko、catch、mania 包会在下载前排除。导入阶段仍只保留 standard 的 `.osu` 文件。
- 每张已导入的源谱面都会保留为 `<数据目录>\beatmaps\<BeatmapID>.osu`；压缩包、音频、图片和其他非 `.osu` 内容不会保留。
- 默认官方导入数据库为 `E:\osudata`，其中同时保存 SQLite、特征、索引与可复用的 `.osu` 源文件。
- 数据和索引不提交到 Git；需要分发时请使用 Release 附件。

## 快速开始

需要安装 Rust 工具链。

```powershell
cargo run -- init .\data
cargo run -- ingest-local .\data .\fixture-pack.zip
cargo run -- reanalyze .\data
cargo run -- normalizer-fit .\data --version 1
cargo run -- index-build .\data --version 1
cargo run -- query .\data 12345 --version 1
```

当分析算法升级、但 `beatmaps/` 中已经保留了源谱面时，使用 `reanalyze` 重算当前分析版本，再依次运行 `normalizer-fit`、`index-build` 和 `doctor`。OPP 当前要求分析版本 `3`（`five-dimension-slider-v3`）。

导入官方谱包需要从已登录的 `osu.ppy.sh` 浏览器标签页导出 Netscape 格式 Cookie。Cookie 文件请保存在仓库外：

```powershell
.\scripts\import-official-packs.ps1 -CookieFile C:\secure\osu-cookies.txt -Release
```

下载可通过 HTTP(S) 或 SOCKS5 代理进行：

```powershell
.\scripts\import-official-packs.ps1 -CookieFile C:\secure\osu-cookies.txt -Proxy http://127.0.0.1:7890 -Release

# 直接使用 CLI 时，同样传入 --proxy
cargo run -- ingest-packs .\data --cookie-file C:\secure\osu-cookies.txt --proxy socks5://127.0.0.1:1080 S1234
```

默认会同时下载 6 个谱包，再顺序导入以保护 SQLite 的单写入者；可按网络情况调整：

```powershell
.\scripts\import-official-packs.ps1 -CookieFile C:\secure\osu-cookies.txt -DownloadConcurrency 12 -BatchSize 24 -Release
```

## 文档

- [实现说明](docs/implementation.md)：数据流程、特征算法、存储格式、索引、导入与恢复机制。
- [命令说明](docs/implementation.md#命令行)：所有 CLI 命令及其用途。

## 验证

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

本项目与 ppy Pty Ltd. 无关。`osu!` 是 ppy Pty Ltd. 的商标。
