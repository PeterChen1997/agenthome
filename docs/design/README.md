# 设计稿

当前版本：**v2.1「会动的栖息地」**（2026-11-05）：`Habitat.dc.html` 是交互子组件，三块画板用 `<dc-import name="Habitat">` 嵌入；本地副本把它替换成 `assets/habitat/processed/composed-*.png` 静态图以便 Playwright 截图。画布：https://claude.ai/artifact/KFXgY6N31ipCE3yoFgmyjY

```
Main.dc.html / Dashboard.dc.html / MenuBar.dc.html / canvas.json   画板源文件（引用 assets/processed/）
assets/                 生图模型原始输出（habitat、avatar-*、app-icon）
assets/processed/       背景归一为 #0F1423 并缩放后的头像与图标
assets/habitat/         栖息地分层素材：raw-* 原图、processed/ 三时段背景 + 15 张透明精灵 + 合成图
png/                    Playwright 渲染的 2x 截图
brainstorm/             六个方向的概念图与取舍笔记
v1/                     第一版（浅色 macOS 原生风）存档
```

本地预览：在本目录 `python3 -m http.server 18765` 后打开 `http://127.0.0.1:18765/Main.dc.html`（`file:` 协议下 Google Fonts 仍可用，但部分浏览器会拦图片）。

设计令牌见画布右下角便签，或 `canvas.json` 的 `notes.tokens`。
