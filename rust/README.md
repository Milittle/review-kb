# `review-kb`(Rust 实现)

本目录是 `review-kb` CLI 的 **Rust 实现**,与仓库根目录的 Python 实现(`src/review_kb/`)**字节兼容、可互换**。调用方把 PATH 里的 `review-kb` 从 Python 版换成 Rust 版(或反过来),参数、JSON 协议、退出码、数据库文件、checklist 格式全部不变。

命令用法、Agent 接入流程、错误恢复策略请参考仓库根目录的 [README.md](../README.md) 和 [接入与运维指南](../docs/integration-guide.md)——本文只讲 Rust 版的构建、安装与兼容性细节。

## 1. 环境要求

- Rust 工具链,edition 2021,`rust-version = 1.75`(见 `Cargo.toml`)
- SQLite 通过 `rusqlite` 的 `bundled` feature **静态编入二进制**,不需要主机安装 `libsqlite3`
- 构建时不依赖 Python;仅跨二进制一致性测试需要 `uv`(见下文第 5 节)

## 2. 构建

```bash
cd rust
cargo build                 # 调试版 → rust/target/debug/review-kb
cargo build --release       # 发布版 → rust/target/release/review-kb
```

> **必须在 `rust/` 目录下执行 cargo。** 本目录的 `.cargo/config.toml` 把 crates.io 索引指向可用的 sparse 镜像;从仓库根或其他目录用 `cargo --manifest-path` 调用不会读取该覆盖,可能拉取依赖失败。

## 3. 安装到执行机

发布版二进制是单一可执行文件,部署时直接拷贝即可:

```bash
cp rust/target/release/review-kb /usr/local/bin/review-kb
review-kb --help
```

或用 cargo 安装到 `~/.cargo/bin`:

```bash
cd rust
cargo install --path .        # 安装到 ~/.cargo/bin/review-kb
```

执行机不需要 Python、`uv` 或任何 Python 依赖,也不需要系统 SQLite 库。

## 4. PATH 冲突说明(重要)

Rust 二进制和 Python 控制台脚本**同名,都叫 `review-kb`**。如果两者都在 `PATH` 中,实际调用哪个取决于 `PATH` 中的先后顺序。排查方式:

```bash
which -a review-kb            # 列出所有同名二进制及其顺序
review-kb --help              # 确认当前命中的是哪一个
```

生产环境建议**只保留其一**,或在自动化脚本里用**绝对路径**调用,避免歧义。两者输出完全一致,混用同一数据库也没有问题(见下文)。

## 5. 与 Python 版的兼容性

两者可互换,已通过自动化一致性门禁验证:

- **同一 SQLite 数据库**:Python `prepare` 写入的库,Rust 可直接读/写(反之亦然)。schema、migration、WAL 行为一致。
- **同一 JSON 信封与退出码**:`ok` / `error.code` / `error.details` / `warnings` / `meta`,以及 0/1/2/3/4/5 退出码完全一致。
- **同一三处 SHA-256 哈希**:`content_hash`、`source_rule_hash`、`knowledge_revision` 逐字节相同。
- **同一 checklist 格式**:二级标题 + `yaml review-rule` 围栏的解析(含 setext、ATX 尾 `#`、围栏行号映射)与 Python `markdown-it` 一致。

跨二进制一致性由 `rust/tests/` 下的门禁覆盖,从仓库根执行:

```bash
make compat     # golden 清单语料 + 库级 repository/service 兼容 + CLI stdout/退出码逐字节比对
make test       # Python pytest + Rust cargo test 全量
```

> `make compat` / `cargo test` 中的兼容性测试会 shell 出去调用 `uv run review-kb` 作为对照,所以**跑测试时仍需要 Python 源码树和 `uv`**;生产部署只需要 Rust 二进制本身。

### 两处可接受的差异(非契约)

这些差异只影响诊断文本,不影响程序化契约(退出码、`error.code`、`error.details`、stdout 结构),调用方按根 README 第 6 节"检查退出码与 `ok` 字段"使用时无感知:

1. **用法错误的 stderr 文本**:`--limit` 越界、未知参数等,clap 与 Typer 的提示文案不同,但都是退出码 2 + 空 stdout(无 JSON 信封)。
2. **非法 JSON 的错误 `message` 文本**:`rules get` / `overrides set --input -` 收到无法解析的 JSON 时,`error.code` 仍是 `INVALID_SELECTION`、退出码仍为 2,但 `message` 中内嵌的解析错误串来自各自的标准库(`serde_json` vs Python `json`),文本与定位列号不同。

## 6. 项目结构

```
rust/
  Cargo.toml            二进制名 review-kb;lib 名 review_kb(供集成测试调用)
  .cargo/config.toml    本项目专用的 cargo sparse 镜像覆盖
  src/                  errors/cli/service/repository/checklist/config/description/models/...
  migrations/           与 Python 逐字一致的 SQL(经 include_str! 内嵌)
  tests/
    cli_parity.rs           数据驱动的跨二进制 stdout/退出码比对(12 场景 / ~71 步)
    command_tree_parity.rs  命令树守卫:任一实现新增命令而对方没有则失败
    cli_smoke.rs            全部命令组的结构/退出码冒烟
    golden_checklist.rs     markdown 解析黄金语料
    repository_compat.rs / service_compat.rs   库级跨实现兼容
    compat/                 Python 对照脚本
```
