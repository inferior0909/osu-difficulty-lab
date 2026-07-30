# osu-difficulty-lab

`osu-difficulty-lab` 是一个用于分析 `osu!standard` 谱面的 Rust 工具。

它会把谱面转换成五个难度特征：瞄准、速度、读图、手电和物件重叠，并在本地建立相似谱面索引。你可以用它查找「玩法和手感接近」的谱面，也可以导出数据做研究。

当前版本：`v0.1.0`

## 支持范围

- 只支持 `osu!standard` 和 NoMod。
- 支持导入本地谱包，以及从官方谱包目录下载谱面。
- 源谱面只在导入时临时保存；分析结果、元数据和索引保存在本地 `data/` 目录。
- 数据和索引不提交到 Git；需要分发时请使用 Release 附件。

## 快速开始

需要安装 Rust 工具链。

```powershell
cargo run -- init .\data
cargo run -- ingest-local .\data .\fixture-pack.zip
cargo run -- normalizer-fit .\data --version 1
cargo run -- index-build .\data --version 1
cargo run -- query .\data 12345 --version 1
```

导入官方谱包需要从已登录的 `osu.ppy.sh` 浏览器标签页导出 Netscape 格式 Cookie。Cookie 文件请保存在仓库外：

```powershell
.\scripts\import-official-packs.ps1 -CookieFile C:\secure\osu-cookies.txt -Release
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
