# AgentHome · 给接手 agent 的总索引

> 最后更新 2026-11-08。新会话先读本文件，再按「从哪里开始」挑文档，不必通读全部。

## 一句话

本机所有 AI agent 的"家"：把 Claude Code、Codex 等的会话收进一个长期保存、可搜索的仓库，并把 token 消耗、成本和剩余额度放在同一块仪表盘上；左栏有一间像素小屋，三位住户（Claude 狐狸、Codex 企鹅、Gemini 猫）随真实活动和额度变换状态。Rust + GPUI Kit 桌面应用，macOS 优先。

## 现状

- **v0.1.2 已发布并公证**：https://github.com/PeterChen1997/agenthome/releases （私有仓库 `PeterChen1997/agenthome`，分支 `main`）。
- 已实现：Claude Code 与 Codex 会话全量入库（本机约 2,800 会话 / 35 万条消息）；仪表盘（住户额度、累计 token、到期与重置、30/90 天趋势、今天、按模型 / 项目、归档健康）；归档（按日分组列表、轮次 + Trace 折叠行的会话详情、中文全文搜索）；会动的像素栖息地；左侧固定导航；明暗主题跟随系统；CLI。
- 质量基线：`cargo fmt --check` 干净、`cargo clippy --workspace --all-targets` 零警告、`cargo test --workspace` 48 个全绿。
- **设计稿只落地了约一半**：`docs/10-设计落地清单.md` 审计了 270 项规格，完成 130、部分 51、未开始 83、有意偏离 6。
- **已知两个口径缺陷**（清单第 1 批）：Codex 会话级 `input_includes_cache` 仍为 true（`crates/core/src/providers/codex.rs` 约 179 行），列表里 Codex 成本被二次扣减、偏低（仪表盘按消息算，不受影响）；匹配不到父会话的子代理在归档列表里被静默丢弃（`crates/app/src/archive.rs` 的 `arrange`）。
- **还没做的核心能力**：raw 层原件拷贝（对抗 Claude 30 天自动清理，产品立足点）、文件监听（现在要手动 `agenthome scan`）、菜单栏弹窗、设置页。

## 从哪里开始

| 你要做的事 | 先读 |
|---|---|
| 了解产品和架构 | `docs/01-产品与架构方案.md`（第 2 节存储策略、第 4 节 Provider、第 8 节已拍板决策） |
| 接着开发功能 | `docs/10-设计落地清单.md` §13 选批次 → 按 `docs/11-开发流水线.md` 逐关走 |
| 改 UI / 动画 | `docs/design/README.md`、`docs/04-栖息地状态系统.md`、`docs/09-开发踩坑记录.md` §1 |
| 改数据解析 | `docs/research/03-agent-session-formats.md`、`docs/09-开发踩坑记录.md` §2 |
| 加额度来源 | `docs/research/04-usage-apis.md`、`docs/06-statusline-安装.md` |
| 构建发版 | `docs/08-构建与发布流程.md`（照做即可，坑都写了） |
| 写 gpui 代码 | `~/code_hub/_ref/gpui-kit/skills/gpui-kit/references/coding-guides.md`，先 grep 签名再写 |

## 文档索引

| 文件 | 内容 | 状态 |
|---|---|---|
| `README.md` | 对外简介、常用命令、文档表 | 现行 |
| `docs/00-调研总览.md` | GPUI / GPUI Kit 是什么、为什么火、借什么 | 背景 |
| `docs/01-产品与架构方案.md` | 产品定义、软链 vs 拷贝（结论：拷贝 + 三层仓库）、架构、Provider、路线图、已拍板决策 | 现行（开头有进度说明） |
| `docs/02-P0-验证记录.md` | P0 退出标准、真实数据暴露的问题 | 历史 |
| `docs/03-功能清单.md` | 信息架构与全部功能分解（状态标记写于 P1 前，以 `docs/10-设计落地清单.md` 为准） | 现行 |
| `docs/04-栖息地状态系统.md` | 住户六态状态机、触发条件、动画、素材、gpui 实现 | 现行 |
| `docs/05-P1-开发计划.md` | P1 分块与接口约定 | 已完成，历史 |
| `docs/06-statusline-安装.md` | 让 Claude 额度可见的 statusline 包装脚本安装（用户尚未安装） | 现行 |
| `docs/07-P1-集成记录.md` | P1 集成结果、视觉核对、版本历史、已知偏差（已全部并入 `docs/10-设计落地清单.md`） | 历史 |
| `docs/08-构建与发布流程.md` | 发版全流程 + 踩坑（工具链、签名、公证、Release、验证） | 现行 |
| `docs/09-开发踩坑记录.md` | GPUI、数据解析、额度、生图素材、验证工具的坑 | 现行，持续追加 |
| `docs/10-设计落地清单.md` | **设计稿 + 规格逐项对照代码的验收清单**：270 项的状态、证据（path:line）、缺口，以及按依赖排好的 10 个收尾批次 | 现行，开发主清单 |
| `docs/11-开发流水线.md` | 每项功能从认领到发布的 10 道关卡（G0–G9）与勾选项，含多 agent 并行规则 | 现行，必须遵守 |
| `docs/design/README.md` | 设计稿 v2.1 文件结构、实现与设计稿差异 | 现行 |
| `docs/design/brainstorm/README.md` | 六个视觉方向与取舍（最终选「像素栖息地」） | 历史 |
| `docs/research/` 下的 01~04 | 四份原始调研：GPUI 框架、GPUI Kit 生态、17 种 agent 会话格式、用量 / 配额 API | 参考 |

外部资料：
- 设计画布（含可点 Play 的栖息地交互组件）：https://claude.ai/artifact/KFXgY6N31ipCE3yoFgmyjY
- GPUI 跨项目调研权威版：`~/code_hub/_research/gpui/`（本仓库 `docs/research/` 下的 01、02 是快照）
- GPUI Kit v0.9.0 源码：`~/code_hub/_ref/gpui-kit/`

## 代码地图

```
crates/core   agenthome-core，零 UI 依赖
  model.rs            规范化模型：Session / Message / ContentBlock / Usage / Period / QuotaSnapshot / ProviderActivity …
  providers/          SourceProvider trait；claude_code.rs、codex.rs
  ingest.rs           ingest_all：按 size+mtime 增量，一个事务写会话 + 消息 + sources
  store.rs            SQLite（WAL、FTS5 trigram），SCHEMA_VERSION=3；列表 / 详情 / 仪表盘查询 / 归档健康 / 活动
  pricing.rs(.json)   LiteLLM 定价快照与 estimate_usd
  signals/            claude_statusline、codex_rollout 额度快照
  paths.rs            ~/AgentHome、~/.claude/projects、~/.codex/sessions
  tests/              claude_code.rs、codex_fixture.rs、stats.rs + 合成 fixture
crates/cli    agenthome：scan / sessions / show / stats / quota / activity
crates/app    agenthome-app（gpui-kit =0.9.0）
  main.rs             参数（--db --mock --habitat-demo --appearance）、主题 / 字体注册、开窗
  workspace.rs        外壳：标题栏、左侧固定栏（导航 + 栖息地 + 住户行 + 页面专属区）、页面切换
  archive.rs          会话列表 + 详情；render_sidebar_sections 提供视图 / 项目
  message_row.rs      轮次切分与 Trace 行
  dashboard.rs + dashboard/   仪表盘页
  habitat/            state.rs（纯函数 reducer，有单测）、view.rs（渲染与动画）
  live.rs             LiveFeed：每 5 秒拉活动与额度喂给栖息地
  data.rs             DataSource trait 与 Mock；sqlite_source.rs 真实实现
  assets.rs + assets/ 内置素材、字体（VT323、Pixelify Sans）、主题 pixel-habitat.json
script/       bundle-dev.sh（开发用 .app）、release-macos.sh（发版）、agenthome-statusline.sh
```

数据位置：索引库 `~/AgentHome/index/agenthome.db`（可随时删了重建）；额度信号 `~/AgentHome/signals/`。

## 常用命令

```bash
cargo run -p agenthome-cli --release -- scan
```

```bash
cargo build -p agenthome-app && ./script/bundle-dev.sh && open target/AgentHome.app
```

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets && cargo test --workspace
```

```bash
./script/release-macos.sh 0.1.3
```

## 已拍板的决策（不要重新讨论）

1. 产品名 AgentHome，归档目录 `~/AgentHome`。
2. 存储：**拷贝，绝不软链、绝不动 agent 原目录**；仓库分 raw（原件，不可变）/ index（SQLite，可重建）/ user（星标笔记）三层。
3. 额度只用合规来源：Claude 走 statusline，Codex 走 rollout 快照；**不做** Claude OAuth usage 端点、Codex wham 端点。
4. core 自带 CLI；TUI 以后再说。
5. 首次启动默认归档模式。
6. 外部 Provider 协议：自定义最小协议，只借 ACP / MCP 的 JSON-RPC over stdio 传输层。
7. 视觉：「像素栖息地」暗色为主；吉祥物配色对齐各家图标；导航在左侧固定栏。
8. 发版：不移动已发布的 tag，有问题升补丁号重发；包必须公证。

## 下一步

开发顺序以 `docs/10-设计落地清单.md` §13 的 10 个批次为准，每项按 `docs/11-开发流水线.md` 走完 G0–G9 才算完成。批次摘要：

1. **core 补字段与口径修正**（先修上面两个缺陷）：会话级成本、`parent_session_id`、原件路径与状态、工具退出码、工具输出进全文索引。
2. UI 去掉近似值与占位（依赖 1）：列表成本、子代理折叠、仅归档 / N 天后清理标签、在 Finder 显示。
3. 实时数据通道（依赖 1）：文件监听 + 兜底扫描、app 内增量入库、栖息地事件与 toast 走真实数据。
4. 栖息地规则与视觉令牌（纯 app，可与 1 并行）：其中 2.5、7.29 两条要先请用户决定是否算有意偏离。
5. 持久化：user 层（星标、笔记）与 raw 层原件拷贝（依赖 3）。
6. 设置页（依赖 5）。
7. 额度与仪表盘补齐（依赖 1）：先用真实 rollout 核对 Codex credits 字段；Claude 额度需用户同意后按 `docs/06-statusline-安装.md` 安装脚本。
8. 归档列表与搜索增强（依赖 2）。
9. 菜单栏弹窗（依赖 3、7；先查 gpui 是否支持 status item）。
10. P2 / P3 远期：更多来源（Gemini CLI、OpenCode、Pi）、语义搜索、外部 Provider 协议等。

另有两个待查的边角问题：脚本 nohup 拉起时窗口不重绘；`Workspace` 观察整个 `ArchiveView` 导致的多余重绘。

## 协作约定

- 用中文沟通与写文档；不确定的先问，涉及改用户全局配置（如 `~/.claude/settings.json`）必须先征得同意。
- 提交前跑 fmt / clippy / test 三项；改 UI 必须用 `bundle-dev.sh` + `open` 启动后截图核对，不能只靠编译通过。
- gpui 代码先查 `_ref/gpui-kit` 源码签名，不凭记忆写 API。
- 需要发图或通知时，用户习惯收飞书：`lark-cli im +messages-send --as bot --user-id <用户 open_id>`（open_id 用 `lark-cli auth status` 查），发消息前确认内容。
- 每完成一项，按 `docs/11-开发流水线.md` 的 G7 更新：`docs/10-设计落地清单.md` 的状态与证据、`docs/03-功能清单.md` 的标记、新坑进 `docs/09-开发踩坑记录.md`、发版变化进 `docs/08-构建与发布流程.md`，并同步本文件的「现状」与「下一步」。
