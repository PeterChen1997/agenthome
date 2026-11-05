# AgentHome

本机所有 AI agent 的"家"：把 Claude Code、Codex、Gemini CLI 等散落在各处的对话收进一个长期保存、可搜索、可分析的仓库，并把 OpenAI 与 Claude 的消耗和剩余额度放在同一块仪表盘上。

实验性项目，技术栈 Rust + GPUI Kit，目标 macOS / Windows / Linux。

## 当前状态

P1 集成完成并已用真实数据逐页核对（2026-11-05）：Claude Code 与 Codex 会话全量入库，仪表盘（额度 / 成本 / 趋势）、归档三栏、会动的像素栖息地均已实现。详见 [docs/07-P1-集成记录.md](docs/07-P1-集成记录.md)。

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
| [docs/03-功能清单.md](docs/03-功能清单.md) | 信息架构与功能分解（含 token 总量、趋势、重置信用、到期提醒） |
| [docs/04-栖息地状态系统.md](docs/04-栖息地状态系统.md) | 三位住户的状态机、触发条件、动画、素材清单与 gpui 实现思路 |
| [docs/05-P1-开发计划.md](docs/05-P1-开发计划.md) | P1 分块、接口约定、退出标准 |
| [docs/06-statusline-安装.md](docs/06-statusline-安装.md) | 让 Claude 额度可见：statusline 包装脚本的安装步骤 |
| [docs/07-P1-集成记录.md](docs/07-P1-集成记录.md) | P1 集成现状、真实数据数字、已知偏差与待办 |
| [docs/design/](docs/design/README.md) | 设计稿 v2「像素栖息地」源文件、素材、截图与方向头脑风暴 |
| [docs/research/01-gpui-framework.md](docs/research/01-gpui-framework.md) | GPUI 框架深度调研原始报告 |
| [docs/research/02-gpui-kit-ecosystem.md](docs/research/02-gpui-kit-ecosystem.md) | GPUI Kit 组件库与生态应用原始报告 |
| [docs/research/03-agent-session-formats.md](docs/research/03-agent-session-formats.md) | 17 种 agent 的本地会话存储位置与格式、规范化模型草案 |
| [docs/research/04-usage-apis.md](docs/research/04-usage-apis.md) | 用量 / 配额数据来源、ToS 风险、竞品实现细节 |

阅读顺序：先 00，再 01；原始报告按需查。

GPUI / GPUI Kit 的通用调研（跨项目复用）权威版本在 `~/code_hub/_research/gpui/`，本仓库 `docs/research/01、02` 是快照副本。
