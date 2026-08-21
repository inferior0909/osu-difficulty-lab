# osu-difficulty-lab

`osu-difficulty-lab` 是一个用于分析 `osu!standard` 谱面的 Rust 工具，同时提供完全隔离的 `osu!mania` 相似谱面数据管线。

它会把谱面转换成五个难度特征：瞄准、速度、读图、滑条和物件重叠，并在本地建立相似谱面索引。你可以用它查找「玩法和手感接近」的谱面，也可以导出数据做研究。

当前版本：`v0.3.0`

## 支持范围

- 只支持 `osu!standard` 和 NoMod。
- 支持导入本地谱包，以及从官方谱包目录下载谱面。
- 官方批量下载会保留纯 `osu!standard` 包和所有可能混合的包，因此不会漏掉含 standard 谱面的混合包；纯 taiko、catch、mania 包会在下载前排除。导入阶段仍只保留 standard 的 `.osu` 文件。
- 每张已导入的源谱面都会保留为 `<数据目录>\beatmaps\<BeatmapID>.osu`；压缩包、音频、图片和其他非 `.osu` 内容不会保留。
- 默认官方导入数据库为 `E:\osudata`，其中同时保存 SQLite、特征、索引与可复用的 `.osu` 源文件。
- SQLite 会保存 `Apeuriox/rosu-pp` 的 `pp-rework-202607` 固定快照（`9a073d29`）计算的 NoMod 星数和 0.1★ 分桶；每个桶同时记录五维归一化特征及原始 AR、CS、OD 的分布统计，供 OPP 动态推荐使用。
- 数据和索引不提交到 Git；需要分发时请使用 Release 附件。

mania 管线只接受 NoMod 4K/6K/7K，使用项目内置的纯 Rust 结构应变与键型分析，不调用 osu! 官方难度、Roxy 最终段位模型或 MinaCalc，也不会修改 standard 数据文件。

`v0.3.0` 数据集包含 147,568 张 Analyzer v4 记录。谱面 `2571051`、`2573164`、`2628991` 在固定 rework 快照中单张计算超过 30 秒，因此未进入发布索引并保留在失败清单中。该数据集需要包含 Analyzer v4 runtime 的 OPP（`5c5d2cf` 或更新版本）。

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

当分析算法升级、但 `beatmaps/` 中已经保留了源谱面时，使用 `reanalyze` 重算当前分析版本，再依次运行 `normalizer-fit`、`index-build` 和 `doctor`。OPP 当前要求分析版本 `4`（`five-dimension-slider-rosu-reading-v4`）以及完全一致的 rosu-pp Git 快照。

Analyzer v4 将 Reading 从项目原先的密度基线切换为 rework `rosu-pp` 的原生 Reading 属性。`reanalyze` 会保留 v3 的版本化 raw 记录，但使旧 HNSW 发布物失效，再从保留的 `.osu` 文件生成 v4 数据。append-only raw 文件不会删除，因此中断后可重新运行同一命令续算；在 `normalizer-fit` 和 `index-build` 全部完成前，OPP 会拒绝打开这个半成品数据集。

`reanalyze` 会按谱面 ID 与 SHA-256 跳过已经由当前快照成功分析的记录，并把单张谱面的计算放在可替换工作线程中；超过 30 秒或发生解析错误的谱面会写入 `reanalyze-failures.txt`，不会阻塞剩余数据。命令在存在失败时仍返回错误，发布者应检查清单并确认排除范围后再继续生成索引。

从旧版数据库升级时也必须先运行 `reanalyze` 来补齐星数；如果有效记录缺少星数，`normalizer-fit` 会明确失败，不会生成带错误默认星数的数据集。旧星数桶统计缺少 AR、CS、OD 字段时需要重新运行 `normalizer-fit`。`doctor` 会同时校验归一化记录、主/delta 索引覆盖和完整星数桶统计。

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

## osu!mania Ranked 原始谱面

`scripts/download-ranked-mania.ps1` 是与 standard 分析数据完全隔离的原始语料下载脚本：它通过 osu! OAuth API 枚举所有包含 mania 难度的 Ranked 谱面集，再筛选每个谱面集中实际 `ranked` 的 mania 难度，逐张下载 `.osu` 文件（因此不会漏掉 mixed set）。需要安装 `sqlite3`、在 osu! 账号页创建 OAuth 应用，以及导出已登录浏览器的 Netscape Cookie：

```powershell
.\scripts\download-ranked-mania.ps1 -ClientId <client-id> -ClientSecret <client-secret> -CookieFile C:\secure\osu-cookies.txt
```

首次可用 `-InitializeOnly` 只创建 `E:\osu-mania-ranked\mania-ranked.sqlite` 和 `mania_ranked_beatmaps` 表。完整运行会保存可续跑的目录、catalogue、CSV manifest 及失败清单；不要把它们放进 standard 的 `E:\osudata`。

### mania 分析与相似检索

下载完成后，在同一个语料目录生成独立的 mania 特征、分位归一化和 bucket 索引：

```powershell
cargo run --release -- mania-reanalyze E:\osu-mania-ranked
cargo run --release -- mania-normalizer-fit E:\osu-mania-ranked --version 1
cargo run --release -- mania-index-build E:\osu-mania-ranked --version 1
cargo run --release -- mania-doctor E:\osu-mania-ranked --version 1
```

可按库内 BeatmapID 或任意本地 `.osu` 文件查询；结果默认排除同一 beatmapset：

```powershell
cargo run --release -- mania-query E:\osu-mania-ranked --beatmap-id 1001518 --version 1 --limit 20
cargo run --release -- mania-query E:\osu-mania-ranked --file C:\maps\target.osu --version 1 --include-same-set

cargo run --release -- mania-export-csv E:\osu-mania-ranked E:\osu-mania-ranked\mania-v1.csv --version 1
cargo run --release -- mania-export-parquet E:\osu-mania-ranked E:\osu-mania-ranked\mania-v1.parquet --version 1
```

查询只比较相同键数，先从目标难度分位层取候选；若候选总量或同模式族样本过少，会自动向相邻难度层扩展，再按强度组成、键型占比、结构统计、总体难度、BPM 与有效时长精确排序。实现细节和磁盘格式见 [实现说明](docs/implementation.md#osumania-相似谱面管线)。参考算法与许可记录见 [第三方说明](THIRD_PARTY_NOTICES.md)。

## 验证

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

本项目与 ppy Pty Ltd. 无关。`osu!` 是 ppy Pty Ltd. 的商标。
