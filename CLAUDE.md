# AgentHome · 给接手 agent 的总索引

> 最后更新 2026-11-10（v0.1.6 开源准备）。新会话先读本文件，再按「从哪里开始」挑文档，不必通读全部。

## 一句话

本机所有 AI agent 的"家"：把 Claude Code、Codex、Gemini 的会话收进一个长期保存、可搜索的仓库，并把 token 消耗、成本和剩余额度放在同一块仪表盘上；左栏有一间像素小屋，三位住户（Claude 狐狸、Codex 企鹅、Gemini 猫）随真实活动和额度变换状态。Rust + GPUI Kit 桌面应用，macOS 优先。

## 现状

- **v0.1.6 已发布并公证**（2026-11-10；v0.1.5 同日发布，0.1.6 只多了 worktree 会话归属修正）：https://github.com/PeterChen1997/agenthome/releases （私有仓库 `PeterChen1997/agenthome`，分支 `main`）。`/Applications` 里装的仍是 v0.1.4（用户未要求升级）。落地页源码 `site/`，私有预览 https://claude.ai/artifact/N54TGv6kPy71Kkdo4LjhgG ；GitHub Pages 工作流 `.github/workflows/pages.yml` 只手动触发，未启用。本轮：按字节偏移增量入库（大会话追加一行 0.5 ms、改 11 行）、⌘K 全局搜索（高亮 + 跳到消息）、Gemini 接入（Gemini CLI 全量、Antigravity 只取摘要）、成本口径修正（LiteLLM 2026-11-09 价表 + 长上下文分档 + Claude 跨会话按消息 id 去重 + Codex 归档目录，与 ccusage 偏差约 1%，见 `docs/12-成本口径核对.md`）、Claude 额度只走官方 /usage、浅色主题可读性、开源基础文件（LICENSE、贡献指南、CI、中英 README、`site/` 落地页）。
- **开源前待用户拍板**：git 历史 18 个提交的作者邮箱是私人邮箱（改写历史或用孤立根提交另开公开仓库）；仓库可见性；是否启用 GitHub Pages；设计稿 `docs/design/` 里有真实项目名需替换。其余审计项（发版脚本里的签名身份、许可文件随包、内部文档）已处理。
- 已实现：Claude Code 与 Codex 会话全量入库（本机约 2,800 会话 / 35 万条消息）；**实时同步**（app 启动即扫描、库不存在自动建库，notify / FSEvents 监听来源目录、防抖 2 秒只扫变化的来源，每小时兜底全量，标题栏显示「正在入库 x/y」/「监听中 · N 个来源 · X 同步」；新数据入库后仪表盘与归档静默刷新，打开的会话追加新消息）；仪表盘（住户额度、累计 token、到期与重置、30/90 天趋势、今天、按模型 / 项目、归档健康）；归档（按日分组列表、按父会话 id 折叠子代理、会话级成本、轮次 + Trace 折叠行的会话详情、打开即停在最新一条、中文全文搜索）；**设置页基础版**（来源与同步状态 + 立即扫描、Claude statusline 一键安装 / 卸载、归档位置与索引体积、外观跟随系统 / 浅色 / 深色并持久化、合规说明、定价表版本）；**Claude 订阅额度**（后台每 15 分钟在伪终端跑官方 `claude` 的 `/usage` 读屏，桌面版用户也有数据；套餐名读自 `~/.claude.json` 的 organizationType；与终端 statusline 两路合并）；**raw 层原件归档**（每个会话 jsonl 入库前先逐字节归档到 `~/AgentHome/raw/<provider>/<machineId>/<相对路径>/`，zstd 压缩、变长按字节段追加、被改写另存新版本、`manifest.json` 记 sha256 与来源；本机 2,603 个原件 9.8 GB → 占用 3.6 GB，首次 55 秒，之后每次扫描 0.5 秒）；会动的像素栖息地；左侧固定导航；CLI（含 `agenthome statusline status|install|uninstall`）。
- 质量基线：`cargo fmt --check` 干净、`cargo clippy --workspace --all-targets -- -D warnings` 零警告、`cargo test --workspace` 约 130 个全绿（另 3 个 `#[ignore]` 真实数据基准）。索引库 `SCHEMA_VERSION` = 4，**不要升**；老数据修正用 `store.rs` 里带 `meta` 标记的一次性迁移。离屏截图：`--features capture`（锁屏可用，见 `docs/09` §5）。
- **设计稿落地约 55%**：`docs/10-设计落地清单.md` 第二轮（2026-11-08）共 273 项，完成 156、部分 43、未开始 72、有意偏离 6。
- 第一轮审计发现的两个口径缺陷（Codex 会话级 `input_includes_cache`、孤儿子代理被静默丢弃）**已于 2026-11-08 修复**。
- **Claude 额度只走官方 `/usage`**（2026-11-09 用户决定，每 15 分钟必跑）。设置页不再提供 statusline 安装，检测到旧包装脚本时给卸载按钮；用户机器上仍装着，旧快照只在 /usage 无数据时兜底。
- **⚠️ 284 个 Claude 会话的原件已被 Claude Code 的 30 天自动清理删掉（2026-11-08，会话日期 08-19 ~ 09-14），完整内容现在只存在于索引库里。raw 层是在它们被删之后才做好的，**救不回这 284 个原件**：用户已决定不做重建副本，所以升 `SCHEMA_VERSION` 或删库重建会让它们从 app 里永久消失——动之前先跟用户说。** 其余会话的原件自 2026-11-08 起都在 raw 层里。 后台 `/usage` 启动 CLI 时已用 `--settings {"cleanupPeriodDays":36500}` 避免触发清理，但用户自己在终端开 `claude` 仍会清理。
- **还没做的核心能力**：raw 附属文件（tool-results 等，10.47）、user 层持久化（星标重启即丢）、菜单栏弹窗。

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
| `docs/10-设计落地清单.md` | **设计稿 + 规格逐项对照代码的验收清单**：271 项的状态、证据（path:line）、缺口，以及按依赖排好的 10 个收尾批次 | 现行，开发主清单 |
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
  providers/          SourceProvider trait；claude_code.rs、codex.rs、gemini.rs
  ingest.rs           ingest_all：按 size+mtime 增量，一个事务写会话 + 消息 + sources
  store.rs            SQLite（WAL、busy_timeout、FTS5 trigram），SCHEMA_VERSION=4；列表 / 详情 / 仪表盘查询 / 归档健康 / 活动
  watch.rs            notify（FSEvents）监听来源根目录，只认 .jsonl / .jsonl.zst
  statusline_setup.rs Claude statusline 包装脚本安装 / 卸载（改 ~/.claude/settings.json，改前备份）
  pricing.rs(.json)   LiteLLM 定价快照与 estimate_usd
  signals/            claude_statusline、claude_usage（伪终端跑官方 /usage 读屏 + 两路合并）、codex_rollout 额度快照
  paths.rs            ~/AgentHome、~/.claude/projects、~/.codex/sessions、~/.claude/settings.json
  tests/              claude_code.rs、codex_fixture.rs、stats.rs + 合成 fixture
crates/cli    agenthome：scan / sessions / show / stats / quota / activity / statusline
crates/app    agenthome-app（gpui-kit =0.9.0）
  main.rs             参数（--db --mock --habitat-demo --appearance --page --search --session --capture）、主题 / 字体注册、开窗；库不存在时自动建库
  workspace.rs        外壳（Notion 式两栏，无标题栏）：左栏顶行红绿灯 + 立即同步（空白处拖窗口）→ 搜索 → 栖息地与住户 → 导航 → 页面专属区，底部品牌 + 同步状态；页面切换；新数据入库后通知各页刷新
  sync.rs             同步引擎：独立线程 + 自有 Store 连接，启动扫描、监听合并 2–10 秒只处理变化路径、每小时兜底；三个来源
  search_palette.rs   ⌘K 全局搜索面板
  settings.rs         设置页：来源、额度来源（statusline 安装）、归档位置、外观、合规与定价
  config.rs           app 偏好 ~/AgentHome/config/app.json（外观）
  archive.rs          会话列表 + 详情；render_sidebar_sections 提供视图 / 项目
  message_row.rs      轮次切分与 Trace 行
  dashboard.rs + dashboard/   仪表盘页
  habitat/            state.rs（纯函数 reducer，有单测）、view.rs（渲染与动画）
  live.rs             LiveFeed：每 5 秒拉活动与额度，喂给栖息地、住户行与仪表盘额度卡
  data.rs             DataSource trait 与 Mock；sqlite_source.rs 真实实现
  assets.rs + assets/ 内置素材、字体（VT323、Pixelify Sans）、主题 pixel-habitat.json
script/       bundle-dev.sh（开发用 .app）、release-macos.sh（发版）、agenthome-statusline.sh
```

数据位置：索引库 `~/AgentHome/index/agenthome.db`（可随时删了重建）；额度信号 `~/AgentHome/signals/`（`claude-statusline.json`、`claude-usage.json`）；app 偏好 `~/AgentHome/config/app.json`；statusline 包装脚本装到 `~/AgentHome/bin/`。

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
3. 额度只用合规来源：Claude 走官方 `claude` CLI 的 `/usage`（后台伪终端读屏，2026-11-08 用户选定）与 statusline，Codex 走 rollout 快照；**不做** Claude OAuth usage 端点、Codex wham 端点，AgentHome 不读取任何凭据。
4. core 自带 CLI；TUI 以后再说。
5. 首次启动默认归档模式。
6. 外部 Provider 协议：自定义最小协议，只借 ACP / MCP 的 JSON-RPC over stdio 传输层。
7. 视觉：「像素栖息地」暗色为主；logo 为「三住户同屋檐」（`docs/design/logo/`，2026-11-09 选定）；吉祥物配色对齐各家图标；Notion 式两栏、无标题栏，搜索与导航在左栏栖息地下方；栖息地全应用只画一份（左栏）。
8. 发版：不移动已发布的 tag，有问题升补丁号重发；包必须公证。

## 下一步

开发顺序以 `docs/10-设计落地清单.md` §13 的 10 个批次为准（表里有 2026-11-08 的「进度」列），每项按 `docs/11-开发流水线.md` 走完 G0–G9 才算完成。

本轮遗留（批次 1 / 3 / 6 的尾巴）：
- 批次 1：10.31 Codex 工具失败与退出码、10.16 每会话原件状态、5.6 工具输出进全文索引、6.16 估算与真值并列。
- 批次 2：4.12「在 Finder 显示」只差接按钮（`source_path` 已有）；4.8 详情成本标签要区分真值 / 估算；其余 3.12、3.13、4.2、4.3、4.9、4.25、2.11、2.13、2.14、6.36。
- 批次 3：10.13 按 byte offset 增量（现在变化文件整文件重解析）、11.8 CLI `watch`、7.14–7.16 栖息地触发规则（数据已实时，working 待目视确认）、7.23–7.28 toast 走真实事件。
- 批次 6：9.2 来源启停与 ToS 风险标签、9.3 改归档位置 / 重建索引、9.9 theme.json 热加载、7.19、6.10。
- 「从 raw 层重建索引 / 导出 284 个重建副本」2026-11-08 用户判定**不做**（只为在 app 里继续浏览已被清理的会话，没有必要）；代价是升 `SCHEMA_VERSION` 或删库后这 284 个会话会从 app 里消失，升级前要提醒用户。优先做 10.47 附属文件、10.16 每会话「仅归档」状态。
- Claude 额度：设置页「立即刷新」与仪表盘额度卡已截图核对（2026-11-08）；`/usage` 读屏依赖 CLI 2.1.x 的文字格式与未公开的 `CLAUDE_CODE_CHILD_SESSION` 标记，CLI 升级后先跑 `agenthome quota --refresh-claude` 验证。

其余批次：
4. 栖息地规则与视觉令牌（纯 app）：其中 2.5、7.29 两条要先请用户决定是否算有意偏离。
5. 持久化：raw 层 10.1–10.4 已完成；剩 user 层（星标、笔记）、10.5 预览模式、10.47 附属文件。
7. 额度与仪表盘补齐：先用真实 rollout 核对 Codex credits 字段。
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
