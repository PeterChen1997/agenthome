//! 成本估算：内嵌一份精简定价表（`pricing.json`，来源与抓取日期写在该文件头部），
//! 按 docs/research/04-usage-apis.md §4.2 的规则算等价 API 成本。
//!
//! 未知模型返回 `None`，不静默按 0 计。

use crate::model::Usage;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const PRICING_JSON: &str = include_str!("pricing.json");

/// 单位：USD / 1M token。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

#[derive(Deserialize)]
struct Table {
    #[serde(rename = "_fetched")]
    fetched: String,
    models: HashMap<String, Price>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(PRICING_JSON).expect("内嵌 pricing.json 必须合法"))
}

/// 定价表版本（`litellm-<抓取日期>`）。
pub fn pricing_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| format!("litellm-{}", table().fetched))
}

/// 模型名归一化：小写、去 Bedrock/Vertex 前缀、去 `-20250xxx` 日期后缀、去 `[1m]`。
pub fn normalize_model(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    for prefix in [
        "us.anthropic.",
        "eu.anthropic.",
        "apac.anthropic.",
        "anthropic.",
        "anthropic/",
        "openai/",
        "bedrock/",
        "vertex_ai/",
    ] {
        if let Some(rest) = m.strip_prefix(prefix) {
            m = rest.to_string();
            break;
        }
    }
    if let Some(rest) = m.strip_suffix("[1m]") {
        m = rest.to_string();
    }
    if let Some(rest) = m.strip_suffix("-v1:0") {
        m = rest.to_string();
    }
    // Vertex 的 `claude-opus-4-5@20251101`
    m = m.replace('@', "-");
    // 日期后缀 `-20251101`
    if let Some(idx) = m.rfind('-')
        && let Some(tail) = m.get(idx + 1..)
        && tail.len() == 8
        && tail.starts_with("20")
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        m.truncate(idx);
    }
    m
}

/// 查表。精确命中优先；否则去掉非数字的尾段（`-spark` / `-preview` / `-latest`）再试，
/// 数字尾段（版本号）不剥，避免把新版本算成旧价。
pub fn lookup(model: &str) -> Option<Price> {
    let models = &table().models;
    let mut key = normalize_model(model);
    loop {
        if let Some(p) = models.get(&key) {
            return Some(*p);
        }
        let idx = key.rfind('-')?;
        let tail = &key[idx + 1..];
        if tail.chars().any(|c| c.is_ascii_digit()) {
            return None;
        }
        key.truncate(idx);
    }
}

/// 按定价表估算一次 usage 的成本。`input_includes_cache` 为真（Codex）时先把缓存读从 input 里扣掉；
/// Claude 没拆 5m/1h 时整体按 5m 价。
pub fn estimate_usd(model: &str, usage: &Usage) -> Option<f64> {
    let p = lookup(model)?;
    let (input, cache_read) = if usage.input_includes_cache {
        (
            usage.input_tokens.saturating_sub(usage.cache_read_tokens),
            usage.cache_read_tokens,
        )
    } else {
        (usage.input_tokens, usage.cache_read_tokens)
    };
    let (cw5, cw1h) = if usage.cache_write_5m_tokens + usage.cache_write_1h_tokens == 0 {
        (usage.cache_write_tokens, 0)
    } else {
        (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens)
    };
    let f = |n: u64| n as f64 / 1_000_000.0;
    Some(
        f(input) * p.input
            + f(usage.output_tokens) * p.output
            + f(cache_read) * p.cache_read
            + f(cw5) * p.cache_write_5m
            + f(cw1h) * p.cache_write_1h,
    )
}

/// 一组消息的估算成本之和；没有任何可计价消息时为 None。
pub fn estimate_messages(messages: &[crate::model::Message]) -> Option<f64> {
    let mut total = None;
    for m in messages {
        if let (Some(model), Some(u)) = (&m.model, &m.usage)
            && let Some(c) = estimate_usd(model, u)
        {
            total = Some(total.unwrap_or(0.0) + c);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 定价表可解析且覆盖主要系列() {
        for m in [
            "claude-opus-5-5",
            "claude-sonnet-4-6",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "gpt-5",
            "gpt-5-codex",
            "gpt-5-mini",
            "o3",
            "o4-mini",
        ] {
            assert!(lookup(m).is_some(), "{m} 应在定价表内");
        }
        assert!(pricing_version().starts_with("litellm-20"));
    }

    #[test]
    fn 模型名归一化() {
        assert_eq!(
            normalize_model("claude-opus-4-5-20251101"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model("claude-sonnet-4-5@20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model("claude-sonnet-4-6[1m]"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            lookup("gpt-5.3-codex-spark"),
            lookup("gpt-5.3-codex"),
            "非数字尾段可剥"
        );
        assert_eq!(
            lookup("claude-opus-9-9"),
            None,
            "数字尾段不剥，未知模型为 None"
        );
        assert_eq!(lookup("opus"), None);
    }

    #[test]
    fn claude_按缓存分档计价() {
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 2_000_000,
            cache_write_5m_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            ..Usage::default()
        };
        // opus-5-5: 4 + 20 + 0.2 + 5 + 8
        let c = estimate_usd("claude-opus-5-5", &u).unwrap();
        assert!((c - 37.2).abs() < 1e-9, "{c}");
        // 没拆 5m/1h：整体按 5m
        let u2 = Usage {
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            ..u
        };
        let c2 = estimate_usd("claude-opus-5-5", &u2).unwrap();
        assert!((c2 - 34.2).abs() < 1e-9, "{c2}");
    }

    #[test]
    fn codex_input_含缓存要先扣除() {
        let u = Usage {
            input_tokens: 1_000_000,
            cache_read_tokens: 600_000,
            output_tokens: 100_000,
            input_includes_cache: true,
            ..Usage::default()
        };
        // gpt-5-codex: 0.4M*1.25 + 0.6M*0.125 + 0.1M*10 = 0.5 + 0.075 + 1.0
        let c = estimate_usd("gpt-5-codex", &u).unwrap();
        assert!((c - 1.575).abs() < 1e-9, "{c}");
        assert_eq!(estimate_usd("not-a-model", &u), None);
    }
}
