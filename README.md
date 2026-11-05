# AgentHome

本机所有 AI agent 的"家"：把 Claude Code、Codex、Gemini CLI 等散落在各处的对话收进一个长期保存、可搜索、可分析的仓库，并把 OpenAI 与 Claude 的消耗和剩余额度放在同一块仪表盘上。

实验性项目，技术栈 Rust + GPUI Kit，目标 macOS / Windows / Linux。

## 当前状态

P0 验证完成（2026-11-05）：Claude Code 会话已能全量解析进 SQLite 并在 GPUI Kit 三栏界面中浏览与搜索。详见 [docs/02-P0-验证记录.md](docs/02-P0-验证记录.md)。

```bash
cargo run -p agenthome-cli --release -- scan
```

```bash
cargo build -p agenthome-app && ./script/bundle-dev.sh && open target/AgentHome.app
```

## 文档

| 文件 | 内容 |
|---|---|
| [docs/00-调研总览.md](docs/00-调研总览.md) | GPUI / GPUI Kit 是什么、为什么爆火、独特性、风险、我们借什么 |
| [docs/01-产品与架构方案.md](docs/01-产品与架构方案.md) | 产品定义、存储策略（软链 vs 拷贝）、架构、Provider 设计、UI、技术栈、路线图、已拍板决策 |
| [docs/02-P0-验证记录.md](docs/02-P0-验证记录.md) | P0 退出标准逐项结果、真实数据暴露的问题、留给 P1 的事 |
| [docs/research/01-gpui-framework.md](docs/research/01-gpui-framework.md) | GPUI 框架深度调研原始报告 |
| [docs/research/02-gpui-kit-ecosystem.md](docs/research/02-gpui-kit-ecosystem.md) | GPUI Kit 组件库与生态应用原始报告 |
| [docs/research/03-agent-session-formats.md](docs/research/03-agent-session-formats.md) | 17 种 agent 的本地会话存储位置与格式、规范化模型草案 |
| [docs/research/04-usage-apis.md](docs/research/04-usage-apis.md) | 用量 / 配额数据来源、ToS 风险、竞品实现细节 |

阅读顺序：先 00，再 01；原始报告按需查。

GPUI / GPUI Kit 的通用调研（跨项目复用）权威版本在 `~/code_hub/_research/gpui/`，本仓库 `docs/research/01、02` 是快照副本。
