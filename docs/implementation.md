# 实现说明

本文描述 `osu-difficulty-lab v0.1.0` 当前的实现。项目是一个本地运行的 `osu!standard` 谱面分析器和相似度索引，不是在线服务，也不使用 OPP 数据。

## 目标与边界

程序读取 `.osu` 谱面，生成一条可复现的特征记录，然后把记录放进本地索引。查询时先从索引取候选谱面，再按完整距离排序。

当前边界如下：

- 仅处理模式值为 `0` 的 `osu!standard` 谱面。
- 仅支持 NoMod，所有记录的 `mod_profile` 都是 `0`。
- Aim 与 Speed 来自固定版本的 `rosu-pp`。
- Reading、Slider 和 Overlap 是本项目定义的基线算法，不等同于官方星数或任何现有性能计算。
- 谱面压缩包只用于导入，成功后会删除；每个有效的标准谱面会保留为 `beatmaps/<BeatmapID>.osu`，本地同时留下特征、元数据和索引。

## 整体流程

```text
.osu / 官方谱包
       │
       ▼
解析与难度分析 ──► raw-features.bin + metadata.sqlite
       │
       ▼
分位数归一化 ────► features-v<N>.bin + normalizers/v<N>.bin
       │
       ▼
HNSW 构建 ──────► indexes/difficulty-main.hnsw
       │
       ▼
近邻候选 + 精确重排 ──► 相似谱面结果
```

每个阶段都是显式命令。导入数据后需要重新执行归一化和主索引构建，才能让新数据出现在查询结果中。

## 分析版本

记录绑定分析版本，避免在改动算法后静默混用旧数据。当前常量为：

| 项目 | 当前值 |
| --- | --- |
| 分析版本 | `3` |
| 算法 ID | `five-dimension-slider-v3` |
| `rosu-pp` | `4.0.1` |
| Reading | `reading-density-ar-section-v1` |
| Overlap | `overlap-visibility-spatial-strain-v1` |

修改原始特征公式、依赖的难度实现或默认权重时，应提高 `ANALYZER_VERSION`，保留旧记录并用新版本重新分析。归一化版本独立于分析版本；同一批原始记录可以产生多个归一化版本。

## 谱面解析与基础特征

分析器同时使用两条路径：

1. `rosu-pp` 读取谱面并计算 osu 难度属性。
2. 项目内置的轻量解析器读取元数据、AR/OD/CS/HP、首个有效 BPM、物件时间、类型与滑条路径，用于自定义特征。

解析器按时间排序物件，将旗标中的圆、滑条和转盘区分开。最早期官方谱面可能没有 `BeatmapID`；此时程序取完整谱面字节的 SHA-256 前 8 字节作为确定性的本地 ID（若结果为零则使用 `1`）。这样同一文件可重复定位，且不会写入零 ID。

基础特征保存在每条记录中：

| 字段 | 含义 |
| --- | --- |
| `bpm` | 第一个正拍长 Timing Point 换算出的 BPM |
| `ar`、`od`、`cs`、`hp` | 谱面难度设置 |
| `length_seconds` | 最后一个物件结束时间减第一个物件开始时间 |
| `object_count` | 物件总数 |
| `object_density` | 物件数除以时长（个/秒） |
| `circle_ratio`、`slider_ratio`、`spinner_ratio` | 各类物件占总物件数的比例 |
| `max_combo` | `rosu-pp` 给出的最大连击数 |

## 五维难度特征

原始难度向量的顺序固定为：`[aim, speed, reading, slider, overlap]`。

### Aim、Speed

两项分别直接取 `rosu-pp 4.0.1` 在 NoMod 下的 `aim` 和 `speed` 属性。项目不会自行修改这两项的公式。

### Reading

Reading 是一个 400 ms 密度与 AR 压力基线：

1. 忽略转盘，将其余物件按 400 ms 分段计数。
2. 每段密度为 `该段物件数 / 0.4 秒`。
3. 将所有段密度由高到低排序。
4. 第 `i` 个峰值乘以 `0.90^i`，并乘 AR 压力 `1 + clamp(AR, 0, 11) / 10`。
5. 累加后得到 Reading 值。

它刻画短时间内的读取负担与 AR 的共同影响。它不是对视读能力的完整建模，因此应作为研究特征使用。

### Slider

Slider 维度由滑条构成比例和相邻滑条速度变化频率组成：

```text
slider = 0.30 × slider_count / (circle_count + slider_count)
       + 0.70 × changed_slider_speed_transitions / slider_speed_transitions
```

滑条速度依据谱面的 `SliderMultiplier`、当前红线拍长以及继承 Timing Point 的 SV 倍率计算。少于两个滑条时速度变化频率为 `0`。该定义与 OPP 内置运行时的 `five-dimension-slider-v3` 保持一致。

### Overlap

Overlap 评估同时可见或空间干扰的物件对，并输出峰值及辅助统计。

对每个非转盘物件，程序向前检查 3 秒内的非转盘物件。两者的可见区间使用 AR 预读时间：

- `AR < 5`：`preempt = 1800 - 120 × AR` ms；
- `AR ≥ 5`：`preempt = 1200 - 150 × (AR - 5)` ms。

若两个可见区间不重叠，该物件对不会计入。否则计算：

- 时间衰减：`exp(-Δt / 700 ms)`；
- 空间接近：物件圆半径为 `54.4 - 4.48 × CS`，点与滑条分别使用点到折线、折线到折线的最短距离；超过 `1.5` 倍直径比例的物件对被跳过；
- 堆叠压力：距离在 `0.5` 倍直径比例内时，按速度压力增强；
- 滑条遮挡：任一物件是滑条时计入；
- 顺序歧义：空间接近度按物件间隔的平方根衰减；
- 移动交叉：比较相邻移动线段，在线段距离不大于物件半径时，按夹角正弦得到交叉强度。

速度压力为：

```text
sqrt(200 / max(Δt, 50))，限制在 [0.5, 2.0]
```

单个物件对的各项权重依次为：圆/普通空间 `0.35`、滑条遮挡 `0.25`、顺序歧义 `0.15`、堆叠 `0.15`、移动交叉 `0.10`。程序按 400 ms 段聚合，物件强度为 `0.65 × 最大对分数 + 0.35 × ln(1 + 对分数和)`，并按每秒 `0.25` 的基数衰减。段峰值降序后以 `0.90^i` 加权求和，作为五维向量中的 `overlap`。

同时保存：

- `peak`：上述加权峰值；
- `p95`：降序峰值数组中约 95 分位的值；
- `sustained_ratio`：有正 Overlap 分数的物件比例；
- `stack_rate`、`slider_occlusion_rate`、`path_crossing_rate`：相应事件占被比较物件对的比例。

## 归一化

不同难度维度的原始尺度不同，因此索引与查询使用分位数归一化，而不是直接比较原始值。

`normalizer-fit --version N` 会读取当前所有原始记录，为五个维度分别收集数值、排序并去重。一个值的归一化结果是它在该维度有序唯一值列表中的排名，范围为 `0` 到 `1`。规则为：

```text
rank(x) = (最后一个小于等于 x 的下标) / (唯一值数量 - 1)
```

只有一个唯一值时结果为 `0`。归一化器写入 `normalizers/vN.bin`，相应完整记录写入 `features-vN.bin`。这意味着加入数据后重跑同一版本会重写该版本的归一化结果；若要保留可复现的历史分布，请使用新的归一化版本号。

## 相似索引与查询

### 主索引

`index-build --version N` 把 `features-vN.bin` 的五维归一化向量写入 HNSW 主索引：

- 文件：`indexes/difficulty-main.hnsw`；
- 距离：五维欧氏距离的平方（索引只用它选候选）；
- 构建参数：`ef_construction = 200`；
- 标签：与插入顺序一一对应的 `beatmap_id`；
- 校验：同目录写入 SHA-256 文件。

如果不存在 `difficulty-delta.hnsw`，构建时会创建一个空的增量索引。查询会同时读取主索引和增量索引；当前 CLI 只构建主索引，增量索引为后续增量写入预留。

### 查询与精确重排

查询首先读取目标谱面的归一化记录，并从每个索引获取候选。默认 `candidate_limit` 为 256，HNSW 实际搜索请求限制在 64 到 128 个近邻之间。候选去重后，程序排除目标谱面，应用筛选条件，再计算精确距离。

难度距离：

```text
d1 = sqrt(Σ weight_i × (target_i - candidate_i)^2)
```

默认五个难度权重均为 `1.0`。

基础距离由下列差异加权相加：BPM、AR、时长、物件密度使用截断的绝对差；圆与滑条比例直接取绝对差。截断尺度分别是 `300 BPM`、`10 AR`、`300 秒`和`10 个/秒`。默认权重为：

| 特征 | 权重 |
| --- | --- |
| BPM | 0.15 |
| AR | 0.15 |
| 时长 | 0.10 |
| 物件密度 | 0.25 |
| 圆比例 | 0.15 |
| 滑条比例 | 0.20 |

最终距离为：

```text
final = 0.8 × d1 + 0.2 × d2
```

结果按 `final` 升序排列；同分时按 `beatmap_id` 升序。可按 AR 范围、BPM 范围或指定 `beatmapset_id` 过滤。索引的归一化版本必须与查询版本一致，且只接受 NoMod。

## 本地存储

`FeatureStore::open` 会创建以下目录与文件：

| 路径 | 用途 |
| --- | --- |
| `metadata.sqlite` | SQLite 元数据、分析偏移、谱包状态和分析版本 |
| `raw-features.bin` | 原始特征二进制记录 |
| `features-vN.bin` | 第 N 个归一化版本的完整特征记录 |
| `normalizers/vN.bin` | 第 N 个分位数归一化器 |
| `indexes/difficulty-main.hnsw` | 主 HNSW 索引 |
| `indexes/difficulty-*.hnsw.sha256` | 索引校验和 |
| `beatmaps/<BeatmapID>.osu` | 可复用的原始标准谱面；该目录只保存 `.osu` 文件 |
| `tmp/` | 下载中的 `.part` 文件和 7z 解压临时内容 |

二进制特征文件都有 32 字节文件头：原始数据 magic 为 `ODLRAW1`，归一化数据 magic 为 `ODLNORM`，并记录格式版本 `1`。随后是以 bincode 固定长度序列化的记录；SQLite 的偏移量指向每条记录。写入原始记录时，程序先追加二进制记录，再在一个 SQLite 事务中更新元数据和分析表。

SQLite 表：

- `beatmaps`：谱面 ID、谱面集 ID、SHA-256、标题、作者、难度名、制作者、在线链接和更新时间；
- `analyses`：每个谱面、NoMod、分析版本对应的原始/归一化偏移及状态；
- `packs`：官方谱包下载或导入状态、来源地址和最近错误；
- `analysis_versions`：分析版本、算法 ID、依赖/子算法版本和创建时间。

相同 `beatmap_id` 且 SHA-256 不变时不会重复写入。若校验和变化，程序追加新的原始记录并更新该谱面的元数据与偏移。

## 导入官方谱包

`catalog-sync` 抓取标准、精选、比赛、Loved、Chart、主题和艺术家等官方谱包目录，输出谱包 ID。传入 `--standard-only` 时，仅输出官方定义为纯 `osu!standard` 的 `S<数字>` 包 ID。批量脚本则保留完整目录：会直接排除 `ST`、`SM`、`SC` 等纯非标准包，而所有可能混合的包都会下载，以免遗漏其中的 standard 谱面；导入时仍只处理 standard `.osu`。官方公开详情页没有逐谱面模式字段，若要把混合包也精确地在下载前排除，需要另行配置 osu! API OAuth 凭据。`ingest-packs` 需要 Netscape 格式的 `osu.ppy.sh` Cookie：程序从 Cookie 文件中只取 `ppy.sh` 域的键值，按请求附带，不写进数据库。

导入流程：

1. 打开官方谱包详情页，提取已认证的 `packs.ppy.sh` 或 `dl.osu.ppy.sh` 下载地址。
2. 将包下载到 `tmp/<pack-id>.part`。存在的部分文件会用 HTTP Range 尝试续传；服务器返回 `416 Range Not Satisfiable` 时会重新完整下载。
3. 下载进度每约 250 ms 输出一次。慢速连接会继续传输；仅由 HTTP 超时或连接错误触发重试。
4. 每个谱包最多尝试 3 次，退避时间为 2 秒和 4 秒。
5. 支持 ZIP、内嵌 `.osz` 以及 7z 容器；RAR 会明确报为不支持。
6. 仅把能明确识别为非标准模式的谱面标记为跳过；每个成功分析的标准谱面写入 `beatmaps/<BeatmapID>.osu`，不会保存背景、音频、视频或其他资源。
7. 成功导入且没有分析失败时，删除 `.part` 文件并标记谱包为 `complete`。

`download-packs` 仅下载、`ingest-downloaded` 仅导入已下载的包。前者可以通过 `--concurrency N` 并发请求；后者始终顺序运行，避免多个进程同时写 SQLite。脚本 `scripts/import-official-packs.ps1` 默认把完整数据库放在 `E:\osudata`，按批次并发下载后顺序导入，再执行归一化、主索引构建和 `doctor` 健康检查。失败的包 ID 会写入 `failed-pack-ids.txt`，成功的数据仍会被索引。

## 命令行

| 命令 | 作用 |
| --- | --- |
| `init <data-dir>` | 创建或打开本地数据目录与 SQLite schema |
| `catalog-sync --output <file> [--standard-only]` | 同步官方谱包 ID；后者仅保留纯 osu!standard 包 |
| `validate-cookie --cookie-file <file>` | 验证 Netscape Cookie 文件是否包含 `ppy.sh` Cookie |
| `ingest-local <data-dir> <archive>` | 导入本地 ZIP、7z 或兼容压缩包 |
| `ingest-packs <data-dir> --cookie-file <file> <pack-id...>` | 下载并导入指定官方谱包 |
| `download-packs <data-dir> --cookie-file <file> --concurrency N <pack-id...>` | 并发下载未完成的官方谱包到临时目录 |
| `ingest-downloaded <data-dir> <pack-id...>` | 只导入临时目录中已下载的官方谱包，不发起网络请求 |
| `reanalyze <data-dir>` | 从 `beatmaps/*.osu` 重算当前分析版本；算法升级后无需重新下载谱包 |
| `normalizer-fit <data-dir> --version N` | 拟合并写入第 N 个归一化版本 |
| `index-build <data-dir> --version N` | 根据第 N 个归一化版本构建主索引 |
| `query <data-dir> <beatmap-id> --version N --limit N` | 查找相似谱面 |
| `export-csv <data-dir> <output> --version N` | 导出归一化特征 CSV |
| `export-parquet <data-dir> <output> --version N` | 导出归一化特征 Parquet |
| `doctor <data-dir> --version N` | 检查归一化记录和索引是否可打开 |

`catalog-sync`、`ingest-packs` 和 `download-packs` 均可使用 `--proxy <URL>`，支持 HTTP、HTTPS 和 SOCKS5 代理；批处理脚本对应参数为 `-Proxy <URL>`。

推荐的完整顺序：

```powershell
cargo run -- init .\data
cargo run -- ingest-local .\data .\maps.zip
cargo run -- reanalyze .\data
cargo run -- normalizer-fit .\data --version 1
cargo run -- index-build .\data --version 1
cargo run -- doctor .\data --version 1
cargo run -- query .\data 12345 --version 1 --limit 20
```

## 数据发布与复现

仓库的 `.gitignore` 排除了 `data/`、SQLite、下载分片和 Cookie，以防把体积大的派生数据或凭据提交进代码库。若要发布索引，建议将同一批数据的下列文件作为 Release 附件一起提供：

- `metadata.sqlite`；
- `features-vN.bin`；
- `normalizers/vN.bin`；
- `indexes/difficulty-main.hnsw` 与对应 `.sha256`；
- 如已使用增量索引，也提供 `difficulty-delta.hnsw` 与对应 `.sha256`；
- 可选的 `raw-features.bin`，用于重新拟合归一化器。

只发布 `.hnsw` 不足以完成查询：查询还需要同版本的归一化特征文件和 SQLite 元数据。接收方下载后可运行 `doctor` 验证索引及数据可用性。

## 测试

集成测试覆盖了「解析 → 原始记录 → 归一化 → 构建索引 → 查询」主流程，并验证相同谱面更接近、堆叠物件的 Overlap 高于分散物件。分析器单元测试覆盖 Slider 构成比例、红线 BPM 与继承 SV 导致的滑条速度变化。导入模块还覆盖 Cookie 行格式、带认证下载链接提取以及非标准模式识别。

执行：

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
