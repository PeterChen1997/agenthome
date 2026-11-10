# Changelog

All notable changes to AgentHome are listed here. Versions follow [Semantic Versioning](https://semver.org/); while the version is 0.x, minor and patch releases may both contain new features.

Downloads are on the [releases page](https://github.com/PeterChen1997/agenthome/releases). Release builds are for macOS 15+ on Apple Silicon.

## 0.2.0 - 2026-11-10

### Added

- **Menu bar.** A pixel-house icon in the macOS menu bar opens a usage summary: the habitat, today's cost, output and sessions, and one row per agent (Claude, Codex, Gemini) with its status, 5-hour quota left, a 10-cell bar, reset countdown, the tightest other window and today's output and cost. The three most recent sessions open straight into the archive. Buttons open the main window, sync now, open Settings or quit. The popup closes when you click elsewhere.
- **English interface.** Settings → Language offers System, 简体中文 and English; "System" picks Chinese when the macOS preferred language is Chinese and English otherwise. The choice is saved in `~/AgentHome/config/app.json`. Demo data follows the language too. Launch option `--lang zh-CN|en` pins the language without saving it.
- Launch option `--menubar-popup` opens the popup content as a normal window, for screenshots.

### Changed

- Closing the main window now hides it; AgentHome keeps running in the menu bar, syncing and updating quota. Reopen it from the menu bar popup or the Dock icon. Quit with ⌘Q (new app menu), from the popup or from the Dock menu.
- Command-line help and developer error messages of the desktop app are in English.

## 0.1.6 - 2026-11-09

### Fixed

- Sessions that ran in a git worktree inside a repository (`<repo>/.claude/worktrees/agent-…`, also `.codex/worktrees/` and `.worktrees/`) now belong to that repository. Before, each worktree showed up as its own `agent-…` project, which added about 180 entries to the project list. Filtering by a project includes its worktrees. Existing indexes are corrected once on upgrade, without a rebuild.

## 0.1.5 - 2026-11-09

### Added

- **⌘K global search.** A floating palette that searches all messages (SQLite FTS5 trigram, Chinese substrings supported). Results are grouped by session with highlighted matches; ↑ / ↓ and Enter jump to the matching message and highlight it.
- **Gemini source.** Gemini CLI sessions (`~/.gemini/tmp/<hash>/chats/`, `.json` and `.jsonl`) are fully parsed. Antigravity sessions are indexed with title, time and workspace from `conversation_summaries.db`; their message bodies are encrypted and are not read or archived. The Gemini resident and quota card now show "connected, no quota signal".
- Gemini models in the bundled price table.
- Codex archived sessions (`~/.codex/archived_sessions/`) are now ingested. A thread present in both places is counted once.
- The token card explains why cache read is so large.
- App launch options `--page`, `--search` and `--session`, and an offscreen screenshot mode (`--capture`, build with `--features capture`) used for documentation.
- Open-source groundwork: Apache-2.0 license, NOTICE, third-party licenses, contributing guide (English and Chinese), security policy, code of conduct, CI, issue and pull request templates.

### Changed

- **Incremental ingestion by byte offset.** Append-only transcripts are parsed from where the last read stopped, and only changed messages and full-text rows are written. Appending a line to a large session takes about 0.5 ms. The file watcher processes only the paths that changed and batches events over a 2 to 10 second window.
- **Claude quota comes only from the official `/usage`.** It runs every 15 minutes in the background. The old statusline snapshot is used only as a fallback when `/usage` has no data. Settings no longer offers to install the statusline wrapper; when an old installation is detected it offers to uninstall it.
- Cost now defaults to the estimate everywhere, including the "all time" period. Claude Code's own recorded cost is kept for comparison only, because it undercounts resumed sessions.
- Price table updated to the LiteLLM snapshot of 2026-11-09, including long-context price tiers. When the price table changes, stored costs are recalculated automatically.
- Quota polling in the app reduced to every 60 seconds, and quota signals no longer walk the Codex directory every few seconds.
- The archive header shows "N including sub-agents", matching the sidebar count.

### Fixed

- Claude messages duplicated across sessions by fork or resume are counted once (by message id) in totals and statistics.
- Codex requests above the 272k-token long-context threshold are now priced at the higher tier; costs were about 7% too low before.
- Light theme readability: countdowns, session counts and the Codex color on the dashboard; token breakdown now wraps instead of being cut off.
- Codex sessions no longer use the injected `AGENTS.md` content as their title. Existing indexes are corrected once on upgrade, without a rebuild.
- The settings page no longer shows an outdated "Gemini CLI not connected" placeholder.
- The search palette refreshes the window when results arrive; the habitat's event bubble no longer overlaps the online count.

## 0.1.4 - 2026-11-09

### Changed

- Notion-style two-column layout without a title bar. The left sidebar holds window controls and a sync button, search, the habitat and residents, navigation, then page-specific sections, with the app name and sync status at the bottom.
- The habitat is drawn once, in the sidebar; the duplicate on the dashboard was removed, along with dashboard header filters that had no effect.
- New logo: three residents under one roof. The development bundle now includes the app icon.

## 0.1.3 - 2026-11-08

### Added

- **Real-time sync.** On launch the app creates the index if needed and scans all sources, then watches the source directories with FSEvents and ingests changes after a short debounce. A full scan runs every hour as a fallback. The dashboard and archive refresh in place, and an open session appends new messages.
- **Raw archive.** Before indexing, each session transcript is archived byte-for-byte to `~/AgentHome/raw/` with zstd compression. Growing files are stored as appended byte ranges, rewritten files as new versions, with a `manifest.json` recording sha256 and source path. Archived copies survive the agent's own 30-day cleanup.
- **Claude subscription quota** read from the official `claude` CLI's `/usage`, run every 15 minutes in a background pseudo-terminal. It reads no credentials and sends no conversation. The plan name is shown on the quota card.
- **Settings page**: sources and sync status with a scan button, archive location and index size, appearance (follow system, light, dark) saved across restarts, compliance notes, price table version, and one-click install / uninstall of the Claude statusline wrapper (the settings file is backed up first).
- "Reveal in Finder" for a session's original file. Opening a session scrolls to the latest message.
- CLI: `agenthome statusline status|install|uninstall` and `agenthome quota --refresh-claude`.

### Fixed

- Codex session-level token accounting no longer double-counts cached input.
- Sub-agent sessions whose parent could not be matched are no longer dropped; sub-agents are grouped by parent session id.
- Concurrent reads and writes between the app and the sync thread no longer fail with "database is locked".

## 0.1.2 - 2026-11-05

### Changed

- Navigation moved to a fixed left sidebar.

The first notarized release (its download was replaced with a notarized build on 2026-11-08).

## 0.1.1 - 2026-11-05

### Changed

- Navigation bar centered in the window.
- Light and dark themes follow the system appearance; `--appearance light|dark` pins one.

This build was not notarized.

## 0.1.0 - 2026-11-05

First usable build. Not notarized.

### Added

- Ingestion of Claude Code and Codex sessions into a local SQLite index with full-text search.
- Dashboard: quota cards, cumulative tokens, estimated cost from a bundled LiteLLM price table, trends, breakdown by model and by project, archive health.
- Archive: sessions grouped by day, session detail with Markdown rendering and collapsible tool-call rows.
- The animated pixel habitat with three residents.
- CLI: `scan`, `sessions`, `show`, `stats`, `quota`, `activity`.
