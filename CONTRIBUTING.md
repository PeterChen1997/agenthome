# Contributing to AgentHome

[中文版](CONTRIBUTING.zh-CN.md)

Thanks for your interest in AgentHome. This document covers the development setup, the checks every change must pass, and how to add a new session source (Provider).

By participating you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md). Security issues go through the private channel described in [SECURITY.md](SECURITY.md), not public issues.

## Development environment

- **macOS 15 or later** (the app targets macOS first; `LSMinimumSystemVersion` is 15.0). Apple Silicon is the primary test platform.
- **Rust 1.99.1**, pinned by `rust-toolchain.toml`. Running any `cargo` command in the repository installs it through rustup automatically. Do not change your global default toolchain for this project. You need the `rustfmt` and `clippy` components (`rustup component add rustfmt clippy`).
- Xcode Command Line Tools (`xcode-select --install`) for the macOS SDK.

The first build compiles GPUI from source and takes several minutes. The dev profile already enables optimizations for dependencies because an unoptimized GPUI is unusable.

## Repository layout

| Path | What it is |
|---|---|
| `crates/core` | `agenthome-core`: no UI dependencies. Normalized model, providers, ingest, SQLite store, pricing, quota signals. |
| `crates/cli` | `agenthome` command-line tool (`scan`, `sessions`, `show`, `stats`, `quota`, `activity`, `statusline`). |
| `crates/app` | `agenthome-app`, the desktop app built on GPUI Kit. |
| `script/` | Dev bundle, release and statusline helper scripts. |
| `docs/` | Design and architecture documents (mostly in Chinese). |

## Common commands

Ingest local sessions into the index (`~/AgentHome/index/agenthome.db`):

```bash
cargo run -p agenthome-cli --release -- scan
```

Build and launch the app as a dev `.app` bundle:

```bash
cargo build -p agenthome-app && ./script/bundle-dev.sh && open target/AgentHome.app
```

Run the app with mock data (no local sessions needed):

```bash
cargo run -p agenthome-app -- --mock
```

## Checks before you submit

All three must pass. CI runs exactly these on a macOS runner:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

For UI changes, also launch the app with `bundle-dev.sh` + `open` and attach before/after screenshots to the pull request. Compiling is not enough to verify a UI change.

Test fixtures must be synthetic. **Never commit real session transcripts**, real paths under your home directory, or screenshots that show real session titles or project names. `.gitignore` already excludes `crates/core/tests/fixtures/real-*` for local-only fixtures.

## Commit messages

- One logical change per commit.
- Format: `<scope>: <summary>`, where scope is the area touched, for example `core`, `cli`, `app`, `docs`, `ci`. Chinese or English are both fine. Example: `core: add search_messages with highlighted snippets`.
- Explain *why* in the body when it is not obvious from the diff.

## Pull requests

- Open a draft PR early if you want feedback on direction.
- Fill in the PR template: what changed, why, how you verified it (commands run, screenshots for UI).
- Keep PRs focused. Unrelated refactors and formatting changes belong in separate PRs.
- Do not bump the version in `Cargo.toml`; maintainers do that at release time.
- If a change alters the index schema, bump `SCHEMA_VERSION` in `crates/core/src/store.rs` and say so in the PR. A schema bump rebuilds the index from the source files on the next launch, so sessions whose original files the agent has already deleted disappear from the index. Treat schema bumps as user-visible changes.

## Adding a Provider (a new session source)

A Provider teaches AgentHome to read one agent's on-disk session format. The trait is `SourceProvider` in `crates/core/src/providers/mod.rs`:

- `id()` returns the `ProviderId`; `detect()` reports whether the agent's data directory exists; `root()` is the source root used as the raw-archive key.
- `discover()` lists transcript files; `parse_file()` parses one file into the normalized model in `crates/core/src/model.rs` (`Session`, `Message`, `ContentBlock`, `Usage`, ...).
- Optional: `line_parser()` for byte-offset incremental parsing of append-only files, and `accepts()` so the file watcher can decide quickly whether a changed path belongs to this source.

Steps:

1. Read `docs/research/03-agent-session-formats.md` for format notes, and use `providers/claude_code.rs`, `providers/codex.rs` or `providers/gemini.rs` as references.
2. Add `crates/core/src/providers/<name>.rs` and register it in `providers/mod.rs`.
3. Add a synthetic fixture under `crates/core/tests/fixtures/<name>/` and an integration test under `crates/core/tests/`. Cover token usage accounting (input / output / cache) explicitly, since cost numbers depend on it.
4. AgentHome is read-only toward agent data: a Provider must never modify, move or symlink files in the agent's own directories, and must never read credentials or tokens.

## License

By contributing, you agree that your contributions are licensed under the [Apache License 2.0](LICENSE), the same license as the project.
