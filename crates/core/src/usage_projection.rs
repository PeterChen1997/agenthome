//! 本地解析结果 → 远程用量协议 v1 记录（CARM-284 单元 2）。
//!
//! 来源解析器给出 `FileUsage`（`SourceProvider::usage_events`），这里只做三件事：
//! 生成不透明事件键、把 `model::Usage` 换成 v1 的 token 口径、跨文件按事件键去重。
//! token 口径与 `pricing::estimate_usd` 一致，所以 Mac 端按记录逐条计价与本地按消息计价相同。

use crate::cost::Vendor;
use crate::device::DeviceIdentity;
use crate::model::{ProviderId, Usage};
use crate::providers::{FileUsage, UsageBasis};
use crate::usage_v1::{self, EventBasis, Op, UsageRecord};
use std::collections::HashMap;

/// 把一个文件的用量事件变成 v1 记录（`revision = 1`、`Upsert`）。
/// 时间缺失的事件不能入协议（`ts_ms` 必须为正），跳过并记进返回的警告，不静默丢。
pub fn records(
    identity: &DeviceIdentity,
    source: &ProviderId,
    file: &FileUsage,
) -> (Vec<UsageRecord>, Vec<String>) {
    let mut out = Vec::with_capacity(file.events.len());
    let mut warnings = Vec::new();
    for (i, e) in file.events.iter().enumerate() {
        if e.ts_ms <= 0 {
            warnings.push(format!("第 {} 条用量缺少时间，未计入", i + 1));
            continue;
        }
        let basis = match &e.basis {
            UsageBasis::Request(id) => EventBasis::Request(id),
            UsageBasis::Ordinal(n) => EventBasis::Ordinal {
                native_session_id: &file.native_session_id,
                ordinal: *n,
            },
        };
        let model = match e.model.as_deref().filter(|m| usage_v1::is_name(m)) {
            Some(m) => Some(m.to_string()),
            None => {
                if e.model.is_some() {
                    warnings.push(format!("第 {} 条的模型名不合规，按未知模型计", i + 1));
                }
                None
            }
        };
        let vendor = match Vendor::of(model.as_deref().unwrap_or(""), source) {
            Vendor::Other => None,
            v => Some(v.as_str().to_string()),
        };
        let t = tokens(&e.usage);
        if t.reasoning > e.usage.output_tokens {
            warnings.push(format!(
                "第 {} 条的推理 token 大于输出 token，按输出封顶",
                i + 1
            ));
        }
        out.push(UsageRecord {
            v: usage_v1::VERSION,
            event_key: usage_v1::event_key(identity, source, basis),
            revision: 1,
            op: Op::Upsert,
            machine_id: identity.machine_id().to_string(),
            source: source.as_str().to_string(),
            vendor,
            model,
            ts_ms: e.ts_ms,
            requests: u32::try_from(e.usage.requests.max(1)).unwrap_or(u32::MAX),
            input_tokens: t.input,
            output_tokens: e.usage.output_tokens,
            cache_read_tokens: t.cache_read,
            cache_write_5m_tokens: t.cache_write_5m,
            cache_write_1h_tokens: t.cache_write_1h,
            reasoning_tokens: t.reasoning.min(e.usage.output_tokens),
        });
    }
    (out, warnings)
}

struct Tokens {
    input: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    reasoning: u64,
}

/// 与 `pricing::estimate_usd` 同口径：含缓存的 input 先扣缓存读；没拆 5m / 1h 的缓存写整体按 5m。
fn tokens(u: &Usage) -> Tokens {
    let input = if u.input_includes_cache {
        u.input_tokens.saturating_sub(u.cache_read_tokens)
    } else {
        u.input_tokens
    };
    let (cache_write_5m, cache_write_1h) = if u.cache_write_5m_tokens + u.cache_write_1h_tokens == 0
    {
        (u.cache_write_tokens, 0)
    } else {
        (u.cache_write_5m_tokens, u.cache_write_1h_tokens)
    };
    Tokens {
        input,
        cache_read: u.cache_read_tokens,
        cache_write_5m,
        cache_write_1h,
        reasoning: u.reasoning_tokens,
    }
}

/// 同一事件键只留一条：时间最早者（同时间留先出现的），与 `store.rs` 的 `CLAUDE_DEDUP` 同口径。
/// Claude fork / resume 把同一响应复制进多个文件时只计一次；不同请求即使同一毫秒也各自保留。
pub fn dedup(records: impl IntoIterator<Item = UsageRecord>) -> Vec<UsageRecord> {
    let mut out: Vec<UsageRecord> = Vec::new();
    let mut pos: HashMap<String, usize> = HashMap::new();
    for r in records {
        match pos.get(&r.event_key) {
            Some(&i) => {
                if r.ts_ms < out[i].ts_ms {
                    out[i] = r;
                }
            }
            None => {
                pos.insert(r.event_key.clone(), out.len());
                out.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::UsageEvent;

    fn identity() -> (tempfile::TempDir, DeviceIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "h").unwrap();
        (dir, id)
    }

    fn event(basis: UsageBasis, ts_ms: i64, usage: Usage) -> UsageEvent {
        UsageEvent {
            basis,
            ts_ms,
            model: Some("claude-opus-4-5-20251101".into()),
            usage,
        }
    }

    /// 投影出的 token 按定价表计价，必须与本地按消息计价完全相同（Mac 端统一计价的前提）。
    #[test]
    fn tokens_price_like_local_usage() {
        let (_d, id) = identity();
        let cases = [
            // Claude 没拆 5m / 1h：整体按 5m。
            Usage {
                input_tokens: 10,
                output_tokens: 200,
                cache_read_tokens: 5_000,
                cache_write_tokens: 3_000,
                requests: 1,
                ..Usage::default()
            },
            // Claude 拆了两档。
            Usage {
                input_tokens: 10,
                output_tokens: 200,
                cache_read_tokens: 5_000,
                cache_write_tokens: 3_000,
                cache_write_5m_tokens: 1_000,
                cache_write_1h_tokens: 2_000,
                reasoning_tokens: 50,
                requests: 1,
                ..Usage::default()
            },
            // 含缓存口径的 input（防御：解析器目前都已归一成新鲜输入）。
            Usage {
                input_tokens: 9_000,
                output_tokens: 10,
                cache_read_tokens: 8_000,
                requests: 1,
                input_includes_cache: true,
                ..Usage::default()
            },
        ];
        for (i, usage) in cases.into_iter().enumerate() {
            let file = FileUsage {
                native_session_id: "s".into(),
                events: vec![event(UsageBasis::Ordinal(0), 1_000, usage)],
                warnings: vec![],
            };
            let (recs, warnings) = records(&id, &ProviderId::ClaudeCode, &file);
            assert!(warnings.is_empty(), "{warnings:?}");
            let r = &recs[0];
            r.validate().unwrap();
            let back = Usage {
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cache_read_tokens: r.cache_read_tokens,
                cache_write_tokens: r.cache_write_5m_tokens + r.cache_write_1h_tokens,
                cache_write_5m_tokens: r.cache_write_5m_tokens,
                cache_write_1h_tokens: r.cache_write_1h_tokens,
                reasoning_tokens: r.reasoning_tokens,
                requests: 1,
                input_includes_cache: false,
            };
            let model = "claude-opus-4-5-20251101";
            assert_eq!(
                crate::pricing::estimate_usd(model, &back),
                crate::pricing::estimate_usd(model, &usage),
                "case {i}"
            );
            assert_eq!(r.vendor.as_deref(), Some("anthropic"));
        }
    }

    /// 缺时间、模型名不合规、推理大于输出：不静默，进警告；记录仍能通过协议校验。
    #[test]
    fn bad_fields_are_reported() {
        let (_d, id) = identity();
        let usage = Usage {
            output_tokens: 5,
            reasoning_tokens: 9,
            requests: 1,
            ..Usage::default()
        };
        let mut odd = event(UsageBasis::Ordinal(1), 1_000, usage);
        odd.model = Some("model with spaces".into());
        let file = FileUsage {
            native_session_id: "s".into(),
            events: vec![event(UsageBasis::Ordinal(0), 0, usage), odd],
            warnings: vec![],
        };
        let (recs, warnings) = records(&id, &ProviderId::Codex, &file);
        assert_eq!(recs.len(), 1, "缺时间的那条不入协议");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        recs[0].validate().unwrap();
        assert_eq!(recs[0].model, None);
        assert_eq!(recs[0].vendor.as_deref(), Some("openai"), "按来源兜底");
        assert_eq!(recs[0].reasoning_tokens, 5);
    }
}
