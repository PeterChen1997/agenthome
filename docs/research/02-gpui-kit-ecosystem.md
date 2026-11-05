> 快照副本。GPUI 相关调研的权威版本在 `~/code_hub/_research/gpui/`，更新以那里为准。

# GPUI Kit（原 gpui-component）与 GPUI 应用生态调研

调研日期：2026-11-05。数据来源以 `gh api`、crates.io API、gpui-kit 仓库原始文件与官网文档为主；二手博客数据单独标注「未核实」。

---

## 1. gpui-kit 概况与版本

### 1.1 改名事实
| 项目 | 结论 | 来源 |
|---|---|---|
| 仓库 | `longbridge/gpui-component` 已于 **2026-09-03** 重命名为 **`longbridge/gpui-kit`**（旧 URL 由 GitHub 自动 302 跳转，`gh api repos/longbridge/gpui-component` 返回的 full_name 即 `longbridge/gpui-kit`） | PR #2927 "Rebrand to GPUI Kit and move the site to gpui-kit.com"，commit `18922d6`，2026-09-03 |
| 发布 | 改名随 **v0.6.0**（2026-09-03）发布；release note 原话："The repository and ecosystem are now named GPUI Kit... GPUI Component remains the styled component layer within the toolkit." | https://github.com/longbridge/gpui-kit/releases/tag/v0.6.0 |
| 文档站 | 从 gpui-component.com 迁到 **https://gpui-kit.com**（2026-09-04 由 VitePress 迁到 Astro，PR #2926） | 仓库 commit log |
| 关系 | 不是「crate 换名」而是「加壳」：`gpui-kit` 是新的门面 crate，`gpui-component` crate 仍然存在并同步发版 | crates.io |

### 1.2 关键数字（2026-11-05 实测）
- Star：**19,185**；Fork 1,203；open issues/PR 147（其中 open issue 122、closed issue 638、merged PR 2,567）；贡献者 ≥100（API 上限）。创建于 2024-06-13。License 字段为 NOASSERTION（仓库自定义许可文件，使用前需读 LICENSE）。
- 最新 release：**v0.9.0，2026-11-02**。近期节奏：v0.6.0(09-03) → 0.6.1(09-09) → 0.6.4(09-18) → 0.6.6(09-21) → 0.7.0(09-28) → 0.7.1(10-05) → 0.7.3(10-12，0.7.2 被 yank) → 0.8.0(10-19) → 0.8.1(10-26) → 0.9.0(11-02)。**基本每周一版**，minor 版本约每 3 周一次，且 minor 版常带 breaking change。
- crates.io：
  - `gpui-component` 0.9.0，总下载 448,475；
  - `gpui-kit` 0.9.0，总下载 259,940，首发 2026-09-03；
  - `gpui`（Zed 官方）停在 **0.2.2（2025-10-22）**，之后未再发布；
  - `gpui-pre` 0.3.11（2026-11-02），owner 是 **huacnlee**（Longbridge 的 Jason Lee），描述 "gpui-pre snapshot of zed@…"——即 Longbridge 自己每周把 Zed main 的 GPUI 快照发到 crates.io。

### 1.3 与 GPUI 版本的配对关系（重要，易踩坑）
GPUI 存在三条谱系，**不能混用**（混用会出 `E0277` trait 错误）：

| 谱系 | crate | 对应 gpui-kit | 说明 |
|---|---|---|---|
| 旧 crates.io 官方 | `gpui = "0.2.x"` | `gpui-component 0.5.x`（2025-12） | Zed 官方 2025-10 发的 0.2.0~0.2.2，之后 Zed 把框架拆成 gpui / gpui_platform / gpui_web / gpui_macros，官方没再发 crates.io 版 |
| **gpui-pre 周快照（默认）** | `gpui = { package = "gpui-pre", version = "=0.3.11" }` + `gpui_platform = { package = "gpui-pre-platform", ... }` | gpui-kit 0.9.0 → `=0.3.11`；0.8.1 → `=0.3.10`；0.8.0 → `=0.3.9`；0.7.1 → `0.3.8`；0.6.x → 0.3.4~0.3.7 | 从 v0.6.6 起 **精确锁定（=）**，因为 caret 依赖曾让 `cargo update` 升到不兼容快照（issue #3156） |
| gpui-fast（可选） | `gpui-fast 0.1.13` | `gpui-kit = { version = "0.9", features = ["gpui-fast"] }` | Longbridge 的性能分支（保留模式渲染、滚动图层、原生窗口合成），仍标「experimental」 |

**两种接入方式**：
```toml
# 方式 A：crates.io（推荐，一行依赖，GPUI 由 kit 锁定）
[dependencies]
gpui-kit = "0.9"
# 可选：features = ["gpui-fast", "tree-sitter", "tree-sitter-markdown", "tree-sitter-rust", "syntect", "inspector"]

# 方式 B：git（跟 main 走，需要自己锁 Cargo.lock 并保证 GPUI 快照一致）
gpui-kit = { git = "https://github.com/longbridge/gpui-kit", rev = "<sha>" }
```
生态里的做法：Zeron 用自己 fork 的 `zeronsh/zui` + `zeronsh/gpui-component` git rev；Waku 用 `egoist/zed` 的分支；sheprd 用 `gpui-pre =0.3.6`；Wake/TokenMonitor 走 crates.io。

其它环境约束（来源：gpui-kit.com/docs/installation）：Rust ≥ 1.92（Linux 的 oo7 0.6 要求）；macOS 15+ 与 Xcode CLT；Windows 10+ + VS2022 C++ 工作负载 + CMake，**必须 MSVC 工具链**；Linux（Ubuntu 24.04 验证）需 `libfontconfig-dev libwayland-dev libwebkit2gtk-4.1-dev libxkbcommon-x11-dev libx11-xcb-dev libssl-dev libzstd-dev libvulkan1` 等，运行需 Vulkan 驱动。

### 1.4 三层架构（v0.6.0 起）
```
gpui-kit        门面 crate：re-export GPUI + 下面各层，提供 application()/init()/actions!
├── gpui-base   无样式行为层：文本编辑/富文本/选择/Dock/虚拟列表/对话框/Popover/动效/历史/导航
└── gpui-component  完整样式层（shadcn 风格），依赖 gpui-base
gpui-shell      可选：Rust 宿主内嵌 JS 运行时做插件/脚本化应用（0.8.0 可安装、可 bundle）
gpui-webview    可选：macOS/Windows/Linux(X11/XWayland) WebView（wry 系）
gpui-fps        FPS/CPU/内存 HUD
```
官方类比：GPUI ≈ HTML+Tailwind，gpui-base ≈ Base UI，gpui-component ≈ shadcn 样式层。

---

## 2. 组件清单与价值评估

### 2.1 官网 gpui-kit.com/component 的分组（共 69 个文档页，README 宣称 75+ 组件与原语）
- **Basic（31）**：Accordion, Alert, Attachment, Avatar, Badge, Bubble, Button, Checkbox, Collapsible, DropdownButton, Icon, Image, Kbd, Label, Marker, Message, MessageScroller, Pagination, Progress, Questionnaire, Radio, Rating, Skeleton, Slider, Spinner, Stepper, Switch, Tag, TextView, Toggle, Tooltip
- **Form（12）**：Input, Textarea, Editor, Select, Combobox, NumberInput, DatePicker, TimeField, OtpInput, Speech, ColorPicker, Form
- **Layout（13）**：DescriptionList, GroupBox, Root, Theme, Dialog, Notification, Popover, Resizable, Scrollable, Sheet, Sidebar, StatusBar, Toolbar
- **Advanced（13）**：Diff, Calendar, Carousel, Command, Chart, List, Menu, Settings, DataTable, Dock, Tabs, Tree, VirtualList
- 源码目录另含：`native_menu`、`title_bar`、`window_border`、`context menu`（在 `menu/`）、`highlighter`、`plot`（图表原语）、`searchable_list`、`shimmer`、`clipboard`、`inspector`、`touch_selection` 等。
- 进行中：issue/PR **#3636 "ai: Add AI elements for agent and chat interfaces"**（2026-10-22 开）——对标 shadcn AI Elements：Reasoning、Tool、ChainOfThought、Plan、AgentTask、Subagent、PromptInput、ModelSelector、ContextUsage、InlineCitation、Sources、Checkpoint，配套 `examples/chatgpt`。**对「对话归档浏览器」几乎是量身定做，但截至 11-05 尚未合并**（标记：未发布）。

### 2.2 对「用量仪表盘 + 对话归档浏览器」的价值排序
| 等级 | 组件 | 用途 | 备注 |
|---|---|---|---|
| ★★★ | **Chart / Plot**（LineChart、BarChart、AreaChart、PieChart、RadarChart、CandlestickChart、SankeyChart） | 用量趋势、模型/项目占比、成本流向（Sankey 很适合「项目→模型→成本」） | 自带 hover tooltip、crosshair、首帧 1s draw-in 动画、reduced-motion；没有独立 Legend 组件（需自绘）；`point_count` 假设数据连续；多个图共享 id 会串 hover 状态 |
| ★★★ | **TextView(markdown/html)** | 渲染助手回复、CLAUDE.md、工具输出 | 支持 Markdown + 简单 HTML、文本选择、代码块操作、自定义 Markdown 插件、`max_lines` 折叠、流式渲染（`examples/stream-markdown`）、`Bubble` 内自动反色；高亮可选 tree-sitter（每语言一个 feature）或 syntect（~200 语言、几 MB） |
| ★★★ | **Message / MessageScroller / Bubble / Attachment** | 聊天气泡、自动滚底的会话视图 | 已是会话 UI 基础件，#3636 的 AI Elements 将叠在其上 |
| ★★★ | **DataTable / VirtualList / List** | 会话列表（几万条）、按 token/成本排序的表 | 官方宣称「几十万行」虚拟滚动、固定列、可拉伸列、排序、单元格选择；List 有内置搜索与选择状态（`ListState`）；已知 a11y 问题 #3647、Tab 离不开表格 #3788/#3800 |
| ★★☆ | **Sidebar / Dock / Resizable / Tabs** | 左侧项目树 + 中间会话列表 + 右侧详情；Dock 布局可序列化 | Dock 拖拽拆分面板偏重，归档浏览器用 Sidebar+Resizable 足够 |
| ★★☆ | **Tree** | 项目/日期/会话层级 | |
| ★★☆ | **Command（⌘K）/ SearchableList** | 全文搜索会话 | Wake 用同类交互 |
| ★★☆ | **Diff** | 展示 agent 修改的代码 diff（0.7.3 新增，只读、虚拟化、Unified/Split、折叠） | 若归档要显示工具调用的文件改动很有用 |
| ★★☆ | **Settings** | 设置面板（自带搜索） | |
| ★☆☆ | Editor（Tree-sitter + LSP，200K 行）、Calendar/DatePicker（日期范围过滤）、Notification、Dialog/Sheet、Tooltip、Switch/Select、Progress、Skeleton/Shimmer、StatusBar、TitleBar | 常规 | |
| 可不用 | WebView、Speech、Questionnaire、OtpInput、Carousel、Rating、ColorPicker、gpui-shell | | |

---

## 3. 主题与设计系统

- **Tailwind 风格 API**：来自 GPUI 本身，`div().flex().items_center().gap_2().rounded_lg().bg(...)`；gpui-kit 在其上加 `v_flex()/h_flex()`、`Styled`/`StyledExt` 扩展与尺寸（XSmall/Small/Medium/Large）。
- **Root**：`open_window` 自动把视图包进 `Root`，由它持有 Dialog/Sheet/Notification/Popover 等 overlay 层，并执行 `window.set_rem_size(theme.font_size)`（字号即全局缩放）。v0.7.0 起 Root 下沉到 gpui-base。
- **主题系统**（gpui-kit.com/component/theme）：
  - `theme.json`：`colors` 对象 + 点分 token（如 `button.primary.background`、`button.primary.hover.background`），hex 字符串；背景 token 支持两段式 `linear-gradient(135deg, #4F46E5, #06B6D4)`。
  - 代码里 `cx.theme().primary / background / foreground`（shadcn 同名语义 token），`cx.theme().tokens.button_primary` 拿完整 token。
  - `ThemeRegistry::watch_dir` 热加载目录；`Theme::update(cx, |t| t.apply_config(&cfg))` 运行时切换；System/Light/Dark 三组（Follow System）。
  - 内置主题 **38 个预设（36 变体 + Light/Dark）**：Default、Aurora、Ayu、Catppuccin(Latte/Frappe/Macchiato/Mocha)、Everforest、Flexoki、Gruvbox、Hybrid、**macOS Classic Light/Dark**、Mellifluous、Molokai、Solarized、Tokyo Night/Storm/Moon、Twilight 等。
  - v0.9.0 起公共 theme struct 加 `#[non_exhaustive]`（breaking：不能再用 struct 字面量构造）。`Theme::cursor_blink` 可配光标闪烁。
  - 风格：README 称 "Modern controls inspired by macOS and Windows"；有 issue #3051 "Platform-Adaptive Native Theme" 在讨论按平台自动切换；Omarchy 风格另有 `huacnlee/gpui-omarchy`，Material 3 有社区 `mizorewww/gpui-kit-material`。
- **字体**：默认 UI 字体 `.SystemUIFont` 16px，等宽 Menlo/Consolas/DejaVu Sans Mono 13px；可用系统任意字体名，或 `cx.text_system().add_fonts()` 内嵌；官方文档明确要求「用真实 Latin/CJK/emoji 混排内容在每个目标平台验证」。
- **Keybinding**：`KeyBinding::new("cmd-k left", Action, Some("Editor"))` + `cx.bind_keys`，`secondary-s` 跨平台别名需 `KeyBinding::load`；字符串解析失败会 **panic**。`gpui_kit::init(cx)` 必须早于 bind_keys 与开窗。
- **ContextMenu**：`menu/` 下 PopupMenu/ContextMenu/DropdownMenu，`.context_menu(|menu, w, cx| menu.menu("Copy", Box::new(Copy)))`；Linux 平铺 WM 下弹窗定位 bug 在 0.6.4 修复。
- **动效**：`docs/STYLING-AND-MOTION.md` 定义 MotionTokens（spring），reduced-motion 支持。

---

## 4. 文档、示例与质量

- 官网 gpui-kit.com：Docs（40 篇框架文档：action/keybinding/focus/entity/task/i18n/packaging/auto-update/fonts/webassembly/mobile/comparison…）+ Component（69 页）+ Base + Shell；有 `llms.txt / llms-full.txt`（AI 友好）；中英双语（zh-CN）。还有 `skills/gpui-kit/SKILL.md`（Claude Code skill）与 `examples/ai_recipes`（带交互测试的「可执行食谱」，专为 AI 辅助开发设计）。
- Gallery：`crates/story`（workspace 默认成员，`cargo run` 即打开组件画廊），`crates/story-web` 为 WASM 版。
- examples 目录：hello_world、input、editor、syntect、**tig（Git 历史浏览器）**、dock、html、markdown、**stream-markdown**、large-text、webview、system_monitor、sidebar、table_in_scrollable、markdown_table、text_selection、speech、js_* 等 30 个。
- 测试：`gpui_kit::test` 无头 UI 集成测试（0.6.1 起）、AccessKit 可访问性。
- 商业背书：**Longbridge Pro**（新加坡券商长桥的桌面交易终端，macOS Apple Silicon / Windows x64 / Linux x64），README 称「从第一天就用 GPUI Component 构建」。Zed 官方博客 2026-04-27 "Community Champion: Jason Lee" 证实其 Electron 客户端因性能问题用 GPUI 重写，Jason 向 gpui 回贡了 115+ PR。宣传数据「120 FPS」「比 Electron 省 60–80% 内存」来自 Longbridge 自述，**无独立基准（未核实）**。

---

## 5. 已知问题与风险

| 类别 | 现状 | 证据 |
|---|---|---|
| **Breaking change 频率** | 高。周更、minor 每 ~3 周、GPUI 快照每周换；0.7.0/0.9.0 均有 breaking 段；0.7.2 被 yank；gpui-pre API 变化会透过 re-export 传导 | releases 页；Cargo.toml 注释 "any snapshot may change GPUI's API" |
| **Windows** | 公认最弱的目标：TitleBar 上 TabBar 坏（#2018）、Tab 缩进渲染错（#3650）、CRLF 剪贴板（#2877）、TitleBar hover（#2496）、关闭窗口时 VSync RedrawWindow 报错（#3795，11-04）；Paneflow 因 DirectX 稳定性推迟 Windows ARM64；Waku 文档要求 D3D11 FL11_0，VM 下走 Basic Render Driver | gpui-kit issues；baeseokjae 博文；waku/docs/windows.md |
| **Linux** | 需 Vulkan 驱动；Dialog 背景溢出 CSD 阴影区（#3744）、Tooltip 宽度（#3735）、分数缩放下 Textarea 滚动条（#3696）；平铺 WM 弹窗已修；Wayland/X11 双后端；WebView 在 X11 走 WebKitGTK | issues |
| **IME / 中文** | 基础 IME 可用且在持续修：Enter 在合成中误提交（#3255 已修）、候选框跳位（#3286 已修）、CJK 标点换行（#3293 已修）；**仍 open**：macOS 拼音候选框在焦点转到 Dialog 后残留（#3605）、dead-key 合成时 Cmd-Return 提交（#3456）。Wake 宣称全文搜索支持 CJK。 | issues |
| **字体** | `.SystemUIFont` 解析失败曾 panic（#3105，已修）；Linux 跟随系统等宽字体（#3506）；缺字形时依赖 fallback，文档要求逐平台验证 CJK/emoji | fonts.md |
| **性能** | GPUI 默认 debug 构建很慢，所有生态项目都在 `[profile.dev.package."*"] opt-level` 调优（Wake/Zeron 注释原话「gpui 依赖树巨大」）；gpui-fast 给出 Linux 每帧 CPU 3.31ms→0.19ms 的自测；baeseokjae 文指出定时器轮询导致 6–8 次/秒空闲重绘，要改事件驱动 | gpui-fast README；博文 |
| **编译时间** | 依赖树大（tree-sitter 全语言、wry、syntect），首次编译数分钟级；`tree-sitter-languages` 比 syntect 多「几十 MB」 | Cargo features 注释 |
| **许可证** | GitHub 识别为 NOASSERTION，需人工阅读 LICENSE | gh api |
| **a11y** | 大量 open issue（Label 无节点 #3698、DataTable 无表结构 #3647、aria-live #3659） | issues |

### 5.1 同名混淆：iamnbutler/gpuikit
- https://github.com/iamnbutler/gpuikit（**199 star**，作者 Nate Butler，Zed 设计师），crate 名 `gpuikit`，2023-10-04 以 `vitesse` 起家，2024-05 改名 `gpui-kit`，2025-11 去掉连字符改为 `gpuikit`。
- 基于 `gpui-unofficial`（另一套 crates.io 发行）构建，风格偏 SwiftUI；与 Longbridge **无关**。其 `why.md` 直言 Longbridge 2026-09-03 选了「只差一个字符」的名字。
- 结论：搜索 "gpui kit" 时，**带连字符 `gpui-kit` = Longbridge（19k star）；无连字符 `gpuikit` = iamnbutler（199 star）**。

### 5.2 其他相关分支
- `longbridge/gpui-fast`（367★，2026-09-24 起）：Zed GPUI 的性能分支，保留模式 + 滚动图层 + 原生窗口合成。
- `gpui-ce/gpui-ce`（1,460★）：社区版 GPUI fork，「大体 API 兼容上游但正在分化」，加了 transitions/backdrop_blur 等。
- `remorses/gpuix`（2,867★）：Node/Bun 绑定，用 React/Solid 写 GPUI。

---

## 6. GPUI 生态真实应用

Star 数均为 2026-11-05 `gh api` 实测。

### 6.1 AI agent 会话查看器 / 控制台（重点）
| 应用 | Star | 定位 | 技术要点 | 链接 |
|---|---|---|---|---|
| **Zeron** | 3,452 | Claude Code/Codex/Cursor/Devin 等 agent 的原生控制台，macOS/Windows/Linux，自更新，可选多设备同步 | fork 的 GPUI (`zeronsh/zui`) + `gpui-base` git rev；rusqlite(bundled) + notify 7 + tokio；Windows setup.exe 42MB、macOS dmg 89MB | https://github.com/zeronsh/zeron |
| **Wake** | 1,532 | **「所有 AI agent 会话集中浏览/搜索/恢复」**——读 `~/.claude/projects/**/*.jsonl`、`~/.codex`、Cursor、OpenCode 等；全文搜索(CJK)、聊天气泡+折叠工具调用、Insights 热力图/周趋势/Top 模型、导出 Markdown、远程主机、MCP/CLI | crates.io GPUI；**rusqlite 0.32 bundled + notify 7**；macOS 14+，Linux/Windows beta；zip 46MB(mac)/23MB(win)/deb 22MB | https://github.com/iAmCorey/Wake |
| **Waku** | 1,596 | 本地 coding agent 工作台（Amp/Claude Code/Codex/Cursor/OpenCode…），daemon + 桌面 RPC 客户端架构，Git checkpoint 回滚 | `egoist/zed` 分支的 GPUI；daemon 持 SQLite；Sparkle 式 appcast 自更新；Windows setup 22MB | https://github.com/egoist/waku |
| **Ghostex** | 1,596 | 「原生 agents 工作区」，20+ agents，内嵌原生 Ghostty | Rust + GPUI | https://github.com/maddada/Ghostex |
| **sheprd** | 2,169 | Herdr 的原生客户端：终端会话、worktree、agent 管理，三平台 | `gpui-pre =0.3.6`，rusqlite 0.40，含 `sheprd-usage` crate | https://github.com/sheprd-labs/sheprd |
| **diri** | 483 | 并行跑 50 个 coding agent 并统一 review | GPUI | https://github.com/cristicretu/diri |
| **Paneflow** | 93 | agent 终端复用器：持久会话、worktree、diff、内置编辑器；Homebrew/AppImage/deb/rpm/msi + minisign | GPUI + libghostty-vt，Rust 1.98 | https://github.com/arthjean/paneflow |
| **muxel** | 38 | GPUI 原生多 agent 复用器 | | https://github.com/ProjectHax/muxel |
| **TokenMonitor** | 18 | **AI 编程工具 Token 用量追踪桌面应用**（Claude Code/Codex/Gemini CLI/OpenCode/Qoder…）→ SQLite → 用量/成本/配额/趋势图；GPUI 桌面版 + ratatui TUI | Windows MSI **8.3MB**、常驻内存 ~115MB（自述）；MSRV 1.95 | https://github.com/iFence/TokenMonitor |
| awesome-gpui 中其它 AI 类 | — | Arbor、Hadron、Krill、OxiMux、Rabbitty、Reviu、Runner、Shepherd、Vibex、WSP、Farcaster、Lumi(dormant)、MAO、Delta（Zed 团队的 agent 协作环境）、Codux(archived) | | https://github.com/zed-industries/awesome-gpui |

### 6.2 其它代表性 GPUI 应用
| 应用 | Star | 类型 | 链接 |
|---|---|---|---|
| Zed | 92,739 | 代码编辑器（GPUI 发源地） | https://github.com/zed-industries/zed |
| Longbridge Pro | 商业闭源 | 券商交易终端，gpui-kit 的母应用 | https://longbridge.com/desktop |
| OpenLogi | 25,160 | Logitech Options+ 替代（本地优先） | https://github.com/AprilNEA/OpenLogi |
| disktree | 3,989 | 磁盘 treemap（Omarchy） | https://github.com/tobi/disktree |
| Sonora | 2,628 | 音乐流媒体客户端 | https://github.com/sonorahq/sonora |
| nyaterm | 2,269 | 远程终端工作区 | https://github.com/nyakang/nyaterm |
| Zedis | 2,181 | Redis GUI；发 dmg/AppImage/deb/rpm，还提供 `-fast`（gpui-fast）构建，dmg 17MB | https://github.com/vicanso/zedis |
| frame | 2,107 | FFmpeg GUI | https://github.com/66HEX/frame |
| oxideterm | 1,777 | AI-native 终端工作区 | https://github.com/AnalyseDeCircuit/oxideterm |
| Loungy | 1,730 | Spotlight 式启动器（archived） | https://github.com/MatthiasGrandl/Loungy |
| awesome-gpui | 1,552 | 官方生态清单（含 dormant/archived 标记） | https://github.com/zed-industries/awesome-gpui |
| tty7 | 1,440 | 关窗不杀 shell 的终端 | https://github.com/l0ng-ai/tty7 |
| ElyOffice | 1,214 | 离线办公套件 | https://github.com/ELY-Open-Source/ElyOffice |
| look | 1,273 | 跨平台启动器 | https://github.com/kunkka19xx/look |
| GitComet | 999 | Git GUI | https://github.com/Auto-Explore/GitComet |
| scrawl | 872 | 标注截图发给 agent | https://github.com/kitlangton/scrawl |
| quiet-md | 797 | 本地笔记（兼容飞书 DocxXML）macOS/Windows | https://github.com/s1dashu/quiet-md |
| velotype | 629 | Markdown 编辑器 | https://github.com/manyougz/velotype |
| hummingbird | 622 | 音乐播放器 | https://github.com/hummingbird-player/hummingbird |
| dbdelve / dbflux | 659 / 512 | 数据库客户端 | — |
| Argus | 289 | Windows 任务管理器（GPUI Kit） | https://github.com/Ray-lea/Argus |
| Ely-GPUI-Components | 554 | 另一套组件库（浏览器可跑） | https://github.com/ZacharyZhang-NY/Ely-GPUI-Components |

官网还有 `gpui-kit.com/apps` 的「App Stories」页（2026-09-01 PR #2897 添加，页面路径 `website/src/pages/apps.astro`；本次未成功抓取内容，标注未核实）。

---

## 7. 对目标应用（跨三平台「用量仪表盘 + 对话归档仓库」）的可行性评估

### 7.1 总体结论
**可行，且已有 2 个直接同类开源项目验证（Wake = 会话归档浏览器，TokenMonitor = 用量仪表盘），两者都走 crates.io + rusqlite + notify 这条最朴素的路线。** 风险集中在：Windows 成熟度、每周 breaking change、编译时间、以及 AI Elements 尚未合并需自绘部分会话 UI。

### 7.2 分项
| 维度 | 评估 | 依据 |
|---|---|---|
| 开发体验 | 中上。Tailwind 式链式 API + Entity/Context 状态模型上手快；官方有 skill/llms.txt/ai_recipes，AI 辅助开发友好；缺点：GPUI 概念（Entity、Window、Context、Task）与 Rust 生命周期叠加，首周有学习曲线；debug 构建需调 opt-level | README、examples、Wake/Zeron Cargo 注释 |
| 打包分发 | 官方 `docs/packaging.md` 给出三平台路径：macOS `.app`+DMG（Developer ID 签名+公证）、Windows **Inno Setup**、Linux tar.gz/deb。工具选择：cargo-bundle（Zed 用其 fork）或 **cargo-packager 0.11.8**（app/dmg/deb/AppImage/pacman/NSIS/MSI，内置公证）；tauri-bundler 也可但偏重。自更新：官方 `docs/auto-update.md` 建议便携单文件用 `self_update`，安装版交给安装器；Waku 用 appcast(Sparkle 风格)，Zeron/Paneflow 自带更新器与 minisign | packaging.md、auto-update.md、各项目 release |
| 二进制大小 | 实测生态产物：TokenMonitor MSI 8MB / zip 11MB；Zedis dmg 17MB、deb 15MB；Wake win zip 23MB、mac zip 46MB、deb 22MB；Waku win setup 22MB；Zeron 42–98MB（带 daemon/SDK）。**预期目标 app 15–40MB**，远小于 Electron（80–150MB）；tree-sitter 全语言会加几十 MB，用 syntect 或仅 markdown/json 语法可控 | gh releases |
| 启动速度 | 无权威基准。Longbridge 自述「一眨眼」；博客数据 0.1–0.4s（未核实）。原理上无 Chromium/JS 启动，冷启动主要是字体系统初始化 + GPU 上下文；Windows 需 D3D11 | longbridge.com/desktop；博文 |
| SQLite | 生态一致用 **rusqlite(bundled)**（Wake 0.32、Zeron 0.32、sheprd 0.40）；GPUI 自带 `cx.background_executor()`/`Task`，查询放后台任务再 `cx.notify()`；gpui-kit 不内置 ORM/DB | Cargo.toml |
| 文件监听 | gpui-kit workspace 自身就依赖 `notify 8.2.0`（ThemeRegistry::watch_dir 用它）；Wake/Zeron 用 notify 7 监听 `~/.claude/projects/**/*.jsonl` 实时更新 | Cargo.toml |
| Markdown 渲染 | 强项：原生 TextView(markdown/html) + 流式渲染 + 代码块高亮 + 选择复制 + max_lines 折叠；Bubble 内自动反色。已知：inline code 行高不均（#3162 已修）、阿拉伯文换行（#3442 open） | text-view.md |
| 图表 | 7 种图表 + Plot 原语够用（折线/面积/柱状/饼/Sankey）；无 Legend 组件、无缩放/拖拽交互、无热力图（需用 Plot 自绘；Wake 自绘了活动热力图） | chart 文档 |
| 跨平台一致性 | macOS 最稳（Longbridge Pro 主战场）；Linux 需 Vulkan；Windows 问题最多——建议按 Wake 做法「macOS first，Linux/Windows 标 beta」，并在 CI 三平台构建 | issues |
| 版本策略 | 锁 `gpui-kit = "=0.9.0"` 并提交 Cargo.lock；每 3–4 周跟一次 minor，读 release note 的 Breaking 段；避免同时直接依赖 gpui | Cargo.toml 注释 |

### 7.3 与替代方案对比（官方 comparison.md + 本次调研）
- vs Tauri/Electron：体积、内存、IME/键盘链路（Paneflow 从 Tauri 迁走就是因为 webview 处理不好快捷键/IME/焦点）占优；劣势是生态年轻、Windows 坑多、无 HTML 生态。
- vs egui：egui 16 个 widget、立即模式，CJK 需自带字体；gpui-kit 75+、保留状态、系统字体直取。
- vs Iced：Iced 35 widget，Markdown 不支持内联 HTML。
- vs Slint/Qt：Qt 成熟但 C++/QML；Slint 无代码编辑器/语法高亮。

---

## 8. 可借鉴的优秀设计

1. **Wake 的数据层**：只读扫描各 agent 的本地目录（`~/.claude/projects/**/*.jsonl`、`~/.codex/sessions/**/*.jsonl[.zst]` + `state_5.sqlite`、Cursor `state.vscdb`、OpenCode `opencode.db`…）→ rusqlite 索引（可随时重建，star/pin 单独持久化）→ notify 增量更新；提供 MCP + CLI 让 agent 反查历史；Insights（热力图/连续天数/周趋势/Top 模型）。这正是「用量仪表盘 + 归档浏览器」的参考实现。
2. **TokenMonitor 的双前端**：同一数据层同时出 GPUI 桌面版和 ratatui TUI（服务器/无显示器场景 ~18MB 内存），单文件 musl 静态包可跑 CentOS 7。
3. **Waku/Zeron 的 daemon + 客户端分离**：daemon 持 SQLite 与文件系统，GUI 只持展示状态；便于远程主机、崩溃恢复与自更新时替换 GUI。
4. **gpui-kit 自身**：`examples/tig`（Sidebar + Diff + Resizable 的三栏浏览器骨架）、`examples/stream-markdown`（流式 Markdown）、`examples/system_monitor`（实时图表）、`crates/story` 的「Choose theme」菜单与 Settings 组件（带搜索的设置页）。
5. **#3636 AI Elements 的设计原则**：受控 `RenderOnce` 值（应用持有 transcript/streaming 状态，组件只回调）、统一 `Trace` 折叠行（Reasoning/Tool/Plan 共享几何与键盘路径）、状态用「图标+文字」不只靠颜色——即使未合并，也可以照此自绘。
6. **Zedis 的发布矩阵**：同一 tag 同时发 dmg/AppImage/deb/rpm/tar.gz，并提供 `-fast`（gpui-fast）变体 + SHA256SUMS + latest.json。
7. **主题**：直接复用 gpui-kit 的 `theme.json` + `ThemeRegistry::watch_dir`，让用户丢一个 json 进目录即可换肤；macOS Classic 主题可做「原生感」默认。

---

## 9. 参考链接

**官方 / 一手**
- 仓库：https://github.com/longbridge/gpui-kit （旧地址 https://github.com/longbridge/gpui-component 自动跳转）
- Releases：https://github.com/longbridge/gpui-kit/releases （v0.9.0 2026-11-02、v0.6.0 改名说明 2026-09-03）
- 改名 PR：https://github.com/longbridge/gpui-kit/pull/2927
- 文档站：https://gpui-kit.com （/docs/getting-started、/docs/installation、/docs/packaging、/docs/auto-update、/docs/fonts、/docs/keybinding、/docs/comparison、/component、/component/theme、/component/chart、/component/text-view）
- crates.io：https://crates.io/crates/gpui-kit 、https://crates.io/crates/gpui-component 、https://crates.io/crates/gpui-pre 、https://crates.io/crates/gpui
- docs.rs：https://docs.rs/gpui-kit
- AI Elements PR：https://github.com/longbridge/gpui-kit/issues/3636
- gpui-fast：https://github.com/longbridge/gpui-fast
- Longbridge Pro：https://longbridge.com/desktop
- Zed 博客 Community Champion: Jason Lee（2026-04-27）：https://zed.dev/blog/community-champion-jason-lee
- iamnbutler/gpuikit 及其 why.md：https://github.com/iamnbutler/gpuikit 、https://github.com/iamnbutler/gpuikit/blob/main/why.md
- awesome-gpui：https://github.com/zed-industries/awesome-gpui
- gpui-ce：https://github.com/gpui-ce/gpui-ce ；gpuix：https://github.com/remorses/gpuix

**生态应用**
- Wake：https://github.com/iAmCorey/Wake （https://wake.cool）
- Zeron：https://github.com/zeronsh/zeron
- Waku：https://github.com/egoist/waku （docs/windows.md）
- TokenMonitor：https://github.com/iFence/TokenMonitor
- sheprd：https://github.com/sheprd-labs/sheprd
- Paneflow：https://github.com/arthjean/paneflow
- Zedis：https://github.com/vicanso/zedis
- Zed：https://github.com/zed-industries/zed

**二手（已标注未核实）**
- baeseokjae《Native app for coding agents with Rust + GPUI》：https://baeseokjae.github.io/posts/native-app-for-coding-agents-rust-gpui/
- brightcoding 博客（2026-02/06）：https://blog.brightcoding.dev/2026/06/03/stop-wrestling-with-qt-gpui-component-makes-rust-desktop-ui-effortless
- cargo-packager：https://docs.rs/crate/cargo-packager/latest
