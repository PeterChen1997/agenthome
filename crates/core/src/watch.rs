//! 文件监听（落地清单 10.10）：用 notify（macOS 上是 FSEvents）盯住各来源的根目录，
//! 会话文件有变化时告诉调用方「哪个来源的哪些文件变了」。
//!
//! 只负责发现变化，不入库：调用方对变了的路径跑 `ingest_paths`（按字节偏移增量续读），
//! 不再为每次变化遍历整个来源目录；后端要求重扫时报 `Error`，调用方退回全量扫描。

use std::path::{Path, PathBuf};

use notify::{EventKind, RecursiveMode, Watcher as _};

use crate::model::ProviderId;

/// 回调收到的事件。错误不吞，交给调用方展示。
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEvent {
    /// 该来源根目录下这些会话文件被创建、修改、改名或删除。
    Changed {
        provider: ProviderId,
        paths: Vec<PathBuf>,
    },
    /// 监听后端报告的错误（例如 FSEvents 丢事件）。调用方应退回全量扫描。
    Error(String),
}

/// 监听句柄；drop 即停止监听。
pub struct RootWatcher {
    _inner: notify::RecommendedWatcher,
    /// 成功挂上监听的根目录。
    pub watched: Vec<(ProviderId, PathBuf)>,
    /// 没挂上的根目录与原因（目录不存在属于正常情况：该 agent 没装）。
    pub unwatched: Vec<(ProviderId, PathBuf, String)>,
}

/// 递归监听每个根目录。某个根目录挂不上不影响其他根目录，原因记在 `unwatched`。
pub fn watch_roots(
    roots: &[(ProviderId, PathBuf)],
    mut on_event: impl FnMut(WatchEvent) + Send + 'static,
) -> anyhow::Result<RootWatcher> {
    let owned: Vec<(ProviderId, PathBuf)> = roots.to_vec();
    let lookup = owned.clone();
    let mut inner =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) => {
                if event.need_rescan() {
                    // FSEvents 合并或丢了事件，具体哪些文件变了已不可知。
                    on_event(WatchEvent::Error(
                        "文件监听要求重新扫描（事件被合并或丢失）".into(),
                    ));
                    return;
                }
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                let mut hit: Vec<(ProviderId, Vec<PathBuf>)> = Vec::new();
                for path in &event.paths {
                    let Some(provider) = classify(&lookup, path) else {
                        continue;
                    };
                    match hit.iter_mut().find(|(p, _)| *p == provider) {
                        Some((_, paths)) => paths.push(path.clone()),
                        None => hit.push((provider, vec![path.clone()])),
                    }
                }
                for (provider, paths) in hit {
                    on_event(WatchEvent::Changed { provider, paths });
                }
            }
            Err(error) => on_event(WatchEvent::Error(error.to_string())),
        })?;

    let mut watched = Vec::new();
    let mut unwatched = Vec::new();
    for (provider, root) in owned {
        if !root.is_dir() {
            unwatched.push((provider, root, "目录不存在".to_string()));
            continue;
        }
        match inner.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => watched.push((provider, root)),
            Err(error) => unwatched.push((provider, root, error.to_string())),
        }
    }
    Ok(RootWatcher {
        _inner: inner,
        watched,
        unwatched,
    })
}

/// 路径属于哪个来源。只认会话转录文件（`.jsonl` 与 Codex 的 `.jsonl.zst`），
/// 其余文件（锁文件、临时文件、目录本身）的变化不触发入库。
pub fn classify(roots: &[(ProviderId, PathBuf)], path: &Path) -> Option<ProviderId> {
    let name = path.file_name()?.to_str()?;
    if !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
        return None;
    }
    roots
        .iter()
        .find(|(_, root)| path.starts_with(root))
        .map(|(provider, _)| provider.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<(ProviderId, PathBuf)> {
        vec![
            (ProviderId::ClaudeCode, PathBuf::from("/h/.claude/projects")),
            (ProviderId::Codex, PathBuf::from("/h/.codex/sessions")),
        ]
    }

    #[test]
    fn transcript_changes_map_to_their_provider() {
        // ChatGPT Desktop 内置的 Codex 也写在 ~/.codex/sessions，必须能触发 Codex 入库。
        let codex = Path::new("/h/.codex/sessions/2026/11/08/rollout-x.jsonl");
        assert_eq!(classify(&roots(), codex), Some(ProviderId::Codex));
        let zst = Path::new("/h/.codex/sessions/2026/10/01/rollout-y.jsonl.zst");
        assert_eq!(classify(&roots(), zst), Some(ProviderId::Codex));
        let claude = Path::new("/h/.claude/projects/-Users-a-b/abc.jsonl");
        assert_eq!(classify(&roots(), claude), Some(ProviderId::ClaudeCode));
    }

    #[test]
    fn noise_does_not_trigger_ingest() {
        // 锁文件、sqlite、来源目录外的 jsonl 都不该引起扫描，否则活跃时会空转。
        let lock = Path::new("/h/.codex/sessions/2026/11/08/rollout-x.jsonl.lock");
        assert_eq!(classify(&roots(), lock), None);
        let db = Path::new("/h/.codex/state_5.sqlite-wal");
        assert_eq!(classify(&roots(), db), None);
        let outside = Path::new("/h/.codex/history.jsonl");
        assert_eq!(classify(&roots(), outside), None);
    }
}
