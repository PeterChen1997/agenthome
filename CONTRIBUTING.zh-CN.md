# 参与贡献 AgentHome

[English](CONTRIBUTING.md)

感谢你关注 AgentHome。本文说明开发环境、每次改动必须通过的检查，以及如何新增一个会话来源（Provider）。

参与本项目即表示你同意遵守[行为准则](CODE_OF_CONDUCT.md)。安全问题请按 [SECURITY.md](SECURITY.md) 走私密渠道，不要发公开 issue。

## 开发环境

- **macOS 15 及以上**（应用以 macOS 为先，`LSMinimumSystemVersion` 为 15.0），主要在 Apple Silicon 上测试。
- **Rust 1.99.1**，由 `rust-toolchain.toml` 钉版。在仓库里执行任意 `cargo` 命令时 rustup 会自动安装，不要为本项目改全局默认工具链。需要 `rustfmt` 与 `clippy` 组件（`rustup component add rustfmt clippy`）。
- Xcode Command Line Tools（`xcode-select --install`），提供 macOS SDK。

首次构建要从源码编译 GPUI，需要几分钟。dev profile 已经给依赖开了优化，不开优化的 GPUI 慢到不可用。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/core` | `agenthome-core`，零 UI 依赖：规范化模型、Provider、入库、SQLite 索引、定价、额度信号 |
| `crates/cli` | `agenthome` 命令行（`scan`、`sessions`、`show`、`stats`、`quota`、`activity`、`statusline`） |
| `crates/app` | `agenthome-app`，基于 GPUI Kit 的桌面应用 |
| `script/` | 开发包、发版与 statusline 辅助脚本 |
| `docs/` | 设计与架构文档 |

## 常用命令

把本机会话入库（`~/AgentHome/index/agenthome.db`）：

```bash
cargo run -p agenthome-cli --release -- scan
```

构建并以开发版 `.app` 启动：

```bash
cargo build -p agenthome-app && ./script/bundle-dev.sh && open target/AgentHome.app
```

用 mock 数据启动（不需要本机有会话）：

```bash
cargo run -p agenthome-app -- --mock
```

## 提交前三项检查

三项都必须通过，CI 在 macOS runner 上跑的就是这三条：

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

改 UI 时还要用 `bundle-dev.sh` + `open` 启动应用，在 PR 里附改动前后的截图。只靠编译通过不算验证了 UI。

测试 fixture 必须是合成数据。**不要提交真实会话转录**、家目录下的真实路径，或截到真实会话标题、项目名的截图。仅在本地使用的真实 fixture 可以命名为 `crates/core/tests/fixtures/real-*`，`.gitignore` 已经忽略。

## 提交信息

- 一个提交只做一件事。
- 格式：`<范围>：<摘要>`，范围是改动所在的部分，如 `core`、`cli`、`app`、`docs`、`ci`。中英文均可。例：`core：全文搜索接口 search_messages，返回带高亮标记的片段`。
- diff 看不出「为什么」时，在正文里写清楚原因。

## Pull Request

- 想先确认方向，可以尽早开 draft PR。
- 按 PR 模板填写：改了什么、为什么、怎么验证的（跑了哪些命令，UI 改动附截图）。
- 保持 PR 聚焦，无关的重构和格式调整另开 PR。
- 不要改 `Cargo.toml` 里的版本号，发版时由维护者统一升。
- 改动索引表结构时，升 `crates/core/src/store.rs` 的 `SCHEMA_VERSION` 并在 PR 里写明。升版本后下次启动会从原件重建索引，原件已被 agent 自己清理掉的会话会从索引里消失，所以这是用户可见的变化。

## 新增 Provider（会话来源）

Provider 负责读懂某个 agent 在磁盘上的会话格式。trait 是 `crates/core/src/providers/mod.rs` 里的 `SourceProvider`：

- `id()` 返回 `ProviderId`；`detect()` 判断该 agent 的数据目录是否存在；`root()` 是来源根目录，raw 层用原件相对它的路径作归档 key。
- `discover()` 列出转录文件；`parse_file()` 把一个文件解析成 `crates/core/src/model.rs` 里的规范化模型（`Session`、`Message`、`ContentBlock`、`Usage` 等）。
- 可选：`line_parser()` 支持追加写文件按字节偏移增量解析；`accepts()` 让文件监听快速判断变化的路径是否属于该来源。

步骤：

1. 读 `docs/research/03-agent-session-formats.md` 了解格式，参考 `providers/claude_code.rs`、`providers/codex.rs`、`providers/gemini.rs`。
2. 新增 `crates/core/src/providers/<name>.rs`，在 `providers/mod.rs` 里注册。
3. 在 `crates/core/tests/fixtures/<name>/` 放合成 fixture，在 `crates/core/tests/` 加集成测试。token 计量（输入 / 输出 / 缓存）要单独覆盖，成本数字依赖它。
4. AgentHome 对 agent 数据只读：Provider 绝不能修改、移动或软链 agent 自己目录里的文件，也绝不能读取凭据或 token。

## 许可证

提交贡献即表示你同意以本项目相同的 [Apache License 2.0](LICENSE) 授权你的贡献。
