(function () {
  "use strict";

  var LANG_KEY = "agenthome.lang";
  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

  // Asset URLs go through one place so a single-file build can swap in inlined copies.
  var inlined = window.AGENTHOME_ASSETS || {};
  function asset(path) {
    return Object.prototype.hasOwnProperty.call(inlined, path) ? inlined[path] : path;
  }

  function store(key, value) {
    try {
      if (value === undefined) return window.localStorage.getItem(key);
      window.localStorage.setItem(key, value);
    } catch (e) {
      return null;
    }
    return null;
  }

  /* ---------- Language ---------- */

  var zh = {
    "skip": "跳到正文",
    "nav.menubar": "菜单栏",
    "nav.tour": "预览",
    "nav.features": "功能",
    "nav.privacy": "隐私",
    "nav.install": "安装",
    "hero.eyebrow": "v0.2.6 · 开源 · macOS",
    "hero.title": "给你的 AI 编程 agent 一个家。",
    "hero.lede": "AgentHome 把 Claude Code、Codex、Gemini CLI 的每个会话拷贝进本地仓库，长期保存、随时全文搜索；token 消耗、估算成本和订阅剩余额度放在同一块仪表盘上。小屋里的三位住户，会跟着 agent 的真实活动和额度变换状态。",
    "hero.download": "下载 macOS 版",
    "hero.source": "在 GitHub 查看源码",
    "hero.f1": "macOS 15+ · Apple Silicon",
    "hero.f2": "已签名并经 Apple 公证",
    "hero.f3": "中英双语界面",
    "hero.note": "演示循环。应用里的状态跟随真实数据：会话正在写入时工作，任一额度窗口低于 20% 时疲惫，空闲或额度用尽时睡觉。",
    "mb.label": "常驻菜单栏",
    "mb.title": "额度和今天的花费，点一下就看到。",
    "mb.lede": "点 macOS 菜单栏里的像素小屋，不用打开主窗口，就能看到三位 agent 的使用概要。",
    "mb.1.t": "今天一眼看到",
    "mb.1.p": "栖息地，加上今日成本、今日输出 token 和今日会话数。",
    "mb.2.t": "每位 agent 一行",
    "mb.2.p": "Claude、Codex、Gemini：状态、5 小时窗口剩余（10 格体力条）、重置倒计时、最吃紧的另一个窗口，以及今天的输出与成本。",
    "mb.3.t": "接着干活",
    "mb.3.p": "最近 3 个会话点一下直接在归档里打开。还能打开主窗口、立即同步、进设置或退出。",
    "mb.4.t": "一直在线",
    "mb.4.p": "关闭窗口只是隐藏，AgentHome 继续在菜单栏里同步；退出用 ⌘Q。",
    "mb.demo": "弹窗截图为演示数据。",
    "tod.day": "白天",
    "tod.evening": "傍晚",
    "tod.night": "夜晚",
    "tour.label": "看看里面",
    "tour.title": "agent 做过的一切，都在一个窗口里。",
    "tour.t1": "仪表盘",
    "tour.t2": "会话详情",
    "tour.t3": "⌘K 搜索",
    "tour.dark": "深色",
    "tour.light": "浅色",
    "tour.c1": "每位住户的额度，按输入、输出、缓存读、缓存写拆开的累计 token，估算成本，重置倒计时，以及 30 / 90 天趋势。",
    "tour.c2": "会话按日分组、子代理折叠到父会话下。打开后按轮次排开，工具调用折叠成 Trace 行，附 token 数与会话级成本。",
    "tour.c3": "任何地方按 ⌘K：跨全部会话全文搜索，命中处高亮，回车直接跳到原消息。",
    "tour.demo": "截图为演示数据。",
    "feat.label": "它做什么",
    "feat.title": "四件事，认真做好。",
    "feat.1.t": "本地优先的归档",
    "feat.1.p": "Claude Code、Codex、Gemini CLI 的会话被拷贝进 <code>~/AgentHome</code>，绝不移动、绝不软链。agent 30 天后清理自己的日志，你的历史依然在。同步是实时增量的：新消息写下几秒后就能看到。",
    "feat.2.t": "算得清的成本",
    "feat.2.p": "token 用量按 LiteLLM 价表计价，缓存读、缓存写各按自己的单价算。与 ccusage 对照，总额偏差约 1%。可按天、模型、项目或会话拆开看。",
    "feat.2.f": "与 ccusage 偏差 ≈1%",
    "feat.3.t": "额度一眼看到",
    "feat.3.p": "订阅剩余额度与重置倒计时放在同一块仪表盘上。Claude 额度来自官方 <code>claude /usage</code>，Codex 来自它自己的 rollout 快照。额度快见底时，小屋里对应的住户会累。",
    "feat.4.t": "⌘K 找回任何一句话",
    "feat.4.p": "跨全部会话的全局全文搜索，中文也能搜。命中在上下文里高亮，选中即跳到所在会话的原消息。",
    "priv.label": "隐私与合规",
    "priv.title": "会话留在你自己的 Mac 上。",
    "priv.lede": "AgentHome 是本地应用，没有账号，没有服务器。它只读取 agent 已经写到磁盘上的内容。",
    "priv.1.t": "本地优先",
    "priv.1.p": "归档、索引和设置都在本机的 <code>~/AgentHome</code> 下。",
    "priv.2.t": "只读来源",
    "priv.2.p": "只读取并拷贝 agent 的目录，绝不修改、移动或软链它们。",
    "priv.3.t": "不读凭据",
    "priv.3.p": "不读取 token、API key 或登录文件。Claude 额度通过运行官方 <code>claude /usage</code> 获得。",
    "priv.4.t": "无遥测",
    "priv.4.p": "没有统计分析，不上报使用情况。源码公开，可以自己查。",
    "inst.label": "安装",
    "inst.title": "三步，大约一分钟。",
    "inst.1.t": "下载",
    "inst.1.p": "从 GitHub 最新 Release 下载 <code>AgentHome-0.2.6-macos-arm64.zip</code>。",
    "inst.2.t": "拖进应用程序",
    "inst.2.p": "解压后把 <code>AgentHome.app</code> 拖进「应用程序」。安装包已用 Developer ID 签名并经 Apple 公证。",
    "inst.3.t": "打开",
    "inst.3.p": "首次启动会把磁盘上已有的会话建成归档，之后常驻菜单栏，持续监听新会话。",
    "inst.cta": "下载 v0.2.6",
    "inst.req": "需要 macOS 15 或更高版本、Apple Silicon。界面支持简体中文与英文，默认跟随 macOS 语言。",
    "foot.by": "作者 Peter Chen",
    "foot.src": "源码",
    "foot.rel": "版本发布",
    "foot.lic": "Apache-2.0 许可",
    "foot.fine": "字体：<a href=\"assets/fonts/OFL-VT323.txt\">VT323</a> 与 <a href=\"assets/fonts/OFL-PixelifySans.txt\">Pixelify Sans</a>，SIL Open Font License 1.1。与 Anthropic、OpenAI、Google 无隶属关系。"
  };

  var stateNames = {
    en: { working: "working", thinking: "thinking", happy: "done!", tired: "tired", sleeping: "sleeping" },
    zh: { working: "工作中", thinking: "思考中", happy: "完成了", tired: "快没电", sleeping: "休息中" }
  };

  var nodes = Array.prototype.slice.call(document.querySelectorAll("[data-i18n]"));
  var en = {};
  nodes.forEach(function (el) { en[el.getAttribute("data-i18n")] = el.innerHTML; });

  var lang = "en";

  function applyLang(next) {
    lang = next === "zh" ? "zh" : "en";
    var dict = lang === "zh" ? zh : en;
    nodes.forEach(function (el) {
      var key = el.getAttribute("data-i18n");
      if (Object.prototype.hasOwnProperty.call(dict, key)) el.innerHTML = dict[key];
    });
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
    document.querySelectorAll(".lang button").forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-lang") === lang));
    });
    renderStateLabels();
    renderShot();
  }

  document.querySelectorAll(".lang button").forEach(function (b) {
    b.addEventListener("click", function () {
      var next = b.getAttribute("data-lang");
      store(LANG_KEY, next);
      applyLang(next);
    });
  });

  /* ---------- Habitat ---------- */

  var scene = document.querySelector(".scene");
  var slots = {};
  document.querySelectorAll(".slot").forEach(function (s) { slots[s.getAttribute("data-who")] = s; });

  function renderStateLabels() {
    Object.keys(slots).forEach(function (who) {
      var label = document.querySelector('.residents li[data-who="' + who + '"] [data-state-label]');
      if (label) label.textContent = stateNames[lang][slots[who].getAttribute("data-state")];
    });
  }

  function setState(who, state) {
    var slot = slots[who];
    if (!slot || slot.getAttribute("data-state") === state) return;
    var img = slot.querySelector("img");
    var src = asset("assets/habitat/" + who + "-" + state + ".webp");
    if (reduceMotion.matches) {
      img.src = src;
      slot.setAttribute("data-state", state);
      renderStateLabels();
      return;
    }
    slot.classList.add("swap");
    window.setTimeout(function () {
      img.src = src;
      slot.setAttribute("data-state", state);
      renderStateLabels();
      slot.classList.remove("swap");
    }, 180);
  }

  // A scripted day in the cabin: each resident walks its own loop, one change at a time.
  var loops = {
    claude: ["working", "thinking", "working", "happy", "tired", "sleeping"],
    codex: ["thinking", "working", "happy", "sleeping", "working", "tired"],
    gemini: ["sleeping", "working", "thinking", "happy", "sleeping", "tired"]
  };
  var step = { claude: 0, codex: 0, gemini: 0 };
  var order = ["claude", "codex", "gemini"];
  var turn = 0;
  var timer = null;

  // Preload every sprite so swaps do not flash.
  if (!reduceMotion.matches) {
    order.forEach(function (who) {
      loops[who].forEach(function (state) { (new Image()).src = asset("assets/habitat/" + who + "-" + state + ".webp"); });
    });
  }

  function tick() {
    var who = order[turn % order.length];
    turn += 1;
    step[who] = (step[who] + 1) % loops[who].length;
    setState(who, loops[who][step[who]]);
  }

  function startLoop() {
    if (timer || reduceMotion.matches || document.hidden) return;
    timer = window.setInterval(tick, 2600);
  }
  function stopLoop() {
    if (timer) window.clearInterval(timer);
    timer = null;
  }

  // Reduced motion: hold a fixed, readable tableau.
  function resetStatic() {
    setState("claude", "working");
    setState("codex", "thinking");
    setState("gemini", "sleeping");
  }

  function onMotionPref() {
    if (reduceMotion.matches) { stopLoop(); resetStatic(); } else { startLoop(); }
  }
  if (reduceMotion.addEventListener) reduceMotion.addEventListener("change", onMotionPref);
  else if (reduceMotion.addListener) reduceMotion.addListener(onMotionPref);
  document.addEventListener("visibilitychange", function () { if (document.hidden) stopLoop(); else startLoop(); });

  // Time of day: follows the visitor's clock (same hours as the app), switchable.
  function setTod(tod) {
    scene.setAttribute("data-tod", tod);
    document.querySelectorAll(".tod button").forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-tod") === tod));
    });
  }
  var hour = new Date().getHours();
  setTod(hour >= 7 && hour <= 16 ? "day" : hour >= 17 && hour <= 19 ? "evening" : "night");
  document.querySelectorAll(".tod button").forEach(function (b) {
    b.addEventListener("click", function () { setTod(b.getAttribute("data-tod")); });
  });

  /* ---------- Screenshot tour ---------- */

  var shotImg = document.getElementById("shot-img");
  var menubarImg = document.getElementById("menubar-img");
  var shot = "dashboard";
  var lightQuery = window.matchMedia("(prefers-color-scheme: light)");
  var mode = lightQuery.matches ? "light" : "dark";
  var alts = {
    dashboard: "AgentHome dashboard: quota per resident, cumulative tokens, estimated cost and reset countdowns",
    session: "AgentHome session detail: turns, trace rows and per-session cost",
    search: "AgentHome ⌘K search: highlighted matches grouped by session"
  };

  // Screenshot files carry the app's locale code: dashboard-dark.en.webp, dashboard-dark.zh-CN.webp.
  function shotSrc(name, theme) {
    return asset("assets/shots/" + name + "-" + theme + "." + (lang === "zh" ? "zh-CN" : "en") + ".webp");
  }

  function renderShot() {
    if (!shotImg) return;
    var src = shotSrc(shot, mode);
    if (shotImg.getAttribute("src") !== src) {
      shotImg.classList.add("loading");
      shotImg.onload = function () { shotImg.classList.remove("loading"); };
      shotImg.src = src;
    }
    shotImg.alt = alts[shot];
    document.querySelectorAll(".tabs button").forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-shot") === shot));
    });
    document.querySelectorAll(".shot-theme button").forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-mode") === mode));
    });
    document.querySelectorAll(".cap").forEach(function (c) { c.hidden = c.getAttribute("data-cap") !== shot; });
    // The menu bar popup follows the page's own light / dark scheme, not the tour toggle.
    var popup = shotSrc("menubar", lightQuery.matches ? "light" : "dark");
    if (menubarImg && menubarImg.getAttribute("src") !== popup) menubarImg.src = popup;
  }
  document.querySelectorAll(".tabs button").forEach(function (b) {
    b.addEventListener("click", function () { shot = b.getAttribute("data-shot"); renderShot(); });
  });
  var modePicked = false;
  document.querySelectorAll(".shot-theme button").forEach(function (b) {
    b.addEventListener("click", function () { modePicked = true; mode = b.getAttribute("data-mode"); renderShot(); });
  });
  // Until the visitor picks one, screenshots follow the system appearance.
  function onScheme() {
    if (!modePicked) mode = lightQuery.matches ? "light" : "dark";
    renderShot();
  }
  if (lightQuery.addEventListener) lightQuery.addEventListener("change", onScheme);
  else if (lightQuery.addListener) lightQuery.addListener(onScheme);

  /* ---------- Boot ---------- */

  applyLang(store(LANG_KEY) === "zh" ? "zh" : "en");
  renderShot();
  onMotionPref();
})();
