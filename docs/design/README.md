# 设计稿

当前版本：**v2.1「会动的栖息地」**（2026-11-05）：`Habitat.dc.html` 是交互子组件，三块画板用 `<dc-import name="Habitat">` 嵌入；本地副本把它替换成 `assets/habitat/processed/composed-*.png` 静态图以便 Playwright 截图。画布：https://claude.ai/artifact/KFXgY6N31ipCE3yoFgmyjY

```
Main.dc.html / Dashboard.dc.html / MenuBar.dc.html / canvas.json   画板源文件（引用 assets/processed/）
assets/                 生图模型原始输出（habitat、avatar-*、app-icon）
assets/processed/       背景归一为 #0F1423 并缩放后的头像与图标
assets/habitat/         栖息地分层素材：raw-* 原图、processed/ 三时段背景 + 15 张透明精灵 + 合成图
png/                    Playwright 渲染的 2x 截图
brainstorm/             六个方向的概念图与取舍笔记
logo/                   Logo：选定 D「三住户同屋檐」（2026-11-09）。app-icon.png 为 1024 发版源图（macOS 网格留边、圆角外透明），app-icon-256.png 为 app 内用的紧凑版；candidate-* 与 logo-candidates.png 是候选存档
v1/                     第一版（浅色 macOS 原生风）存档
```

本地预览：在本目录 `python3 -m http.server 18765` 后打开 `http://127.0.0.1:18765/Main.dc.html`（`file:` 协议下 Google Fonts 仍可用，但部分浏览器会拦图片）。

设计令牌见画布右下角便签，或 `canvas.json` 的 `notes.tokens`。

## 实现与设计稿的差异（以代码为准）

- 导航：设计稿是标题栏分段按钮，实现改成了**左侧固定导航栏**（用户 2026-11-05 要求）。
- 外壳布局（用户 2026-11-09 要求，参照 Notion）：**没有整条标题栏**，正文从窗口顶端开始。左栏顶行只有红绿灯与右侧「立即同步」图标（空白处可拖动窗口）；其下固定搜索框；再往下是栖息地与住户 → 导航 → 归档页的视图 / 项目；最底部是品牌（图标 + AgentHome）与同步状态，对应 Notion 左下角的工作区行。
- 仪表盘不再放大尺寸栖息地（与左栏重复）；第一行改为全宽三列「额度详情」。
- Logo：用户在 5 个候选里选定 D「三住户同屋檐」（暖橙底、狐狸 / 企鹅 / 猫同在一个像素屋檐下），替换原来的夜景小木屋。
- 仪表盘没有页头时间筛选：趋势卡自带 30 / 90 天，按模型 / 按项目跟随累计 token 卡的周期。每个控件只管它所在的卡片。
- 栖息地动画：gpui 没有旋转 / 滤镜 / 径向渐变，按 `docs/09-开发踩坑记录.md` §1 的替代方式实现。
- 菜单栏弹窗（`MenuBar.dc.html`）尚未实现。
- 明暗主题：实现了跟随系统，设置页可固定浅色 / 深色；设计稿只画了暗色。
- 设置页：设计稿没有画板，按 `docs/03-功能清单.md` §2.5 自行排版（数据来源、额度来源、归档位置、外观、合规与定价五节，2026-11-08）。
- 会话详情：打开会话默认滚到最新一条并跟随尾部（用户 2026-11-08 要求），设计稿未规定。
- 同步状态（现位于左栏底部品牌行的第二行）：除设计稿的「监听中 · N 个来源 · X 前同步」外，扫描时显示「正在入库 来源 x/y」，出错时显示错误文字，悬停看全文。
