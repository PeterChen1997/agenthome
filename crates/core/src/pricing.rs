//! 成本估算：内嵌一份精简定价表（`pricing.json`，来源与抓取日期写在该文件头部），
//! 按 docs/research/04-usage-apis.md §4.2 的规则算等价 API 成本。
//!
//! 未知模型返回 `None`，不静默按 0 计。
//!
//! 口径与 LiteLLM / ccusage 一致（核对记录见 docs/12-成本口径核对.md）：
//! - `estimate_usd` 的 `usage` 必须是**单次请求**的用量：长上下文分档按单次请求的提示长度判断，
//!   把整段会话的累计 usage 传进来会被误判成高价档。
//! - 缓存读按 `cache_read` 单价（Claude 约为输入的 0.1×），缓存写分 5 分钟 / 1 小时两档。
//! - 推理 / 思考 token 已含在 `output_tokens` 里，这里不再另计。

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
    /// 长上下文分档（LiteLLM 的 `*_above_<N>k_tokens`）：单次请求的提示超过阈值时整次请求改用这组单价。
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
}

/// 长上下文档单价，单位同 [`Price`]。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct LongContextPrice {
    /// 提示 token（新鲜输入 + 缓存读 + 缓存写）严格大于它时生效。
    pub above_tokens: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

impl Price {
    /// 单次请求实际适用的单价：提示超过长上下文阈值时取高价档，否则取基础价。
    fn rates_for_prompt(&self, prompt_tokens: u64) -> Rates {
        match self.long_context {
            Some(lc) if prompt_tokens > lc.above_tokens => Rates {
                input: lc.input,
                output: lc.output,
                cache_read: lc.cache_read,
                cache_write_5m: lc.cache_write_5m,
                cache_write_1h: lc.cache_write_1h,
            },
            _ => Rates {
                input: self.input,
                output: self.output,
                cache_read: self.cache_read,
                cache_write_5m: self.cache_write_5m,
                cache_write_1h: self.cache_write_1h,
            },
        }
    }
}

struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
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
    // OpenAI 的日期后缀 `-2025-08-07`
    if let Some(idx) = m.len().checked_sub(11)
        && let Some(tail) = m.get(idx..)
        && tail.starts_with("-20")
        && tail.char_indices().all(|(i, c)| {
            if i == 5 || i == 8 {
                c == '-'
            } else {
                i == 0 || c.is_ascii_digit()
            }
        })
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

/// 按定价表估算**单次请求**的成本。`input_includes_cache` 为真时先把缓存读从 input 里扣掉；
/// Claude 没拆 5m/1h 时整体按 5m 价。提示（新鲜输入 + 缓存读 + 缓存写）超过模型的长上下文阈值时，
/// 整次请求（含输出）按高价档计，与 LiteLLM / ccusage 同口径。
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
    let r = p.rates_for_prompt(input + cache_read + cw5 + cw1h);
    let f = |n: u64| n as f64 / 1_000_000.0;
    Some(
        f(input) * r.input
            + f(usage.output_tokens) * r.output
            + f(cache_read) * r.cache_read
            + f(cw5) * r.cache_write_5m
            + f(cw1h) * r.cache_write_1h,
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
        // OpenAI 的 `-YYYY-MM-DD` 快照名要落到同一个价，否则带日期的模型名会变成「未知模型」。
        assert_eq!(normalize_model("gpt-5-2025-08-07"), "gpt-5");
        assert_eq!(normalize_model("o4-mini-2025-04-16"), "o4-mini");
        assert_eq!(normalize_model("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn 定价表单价自洽() {
        // 这些不等式是厂商定价的结构性事实；一旦被破坏，多半是生成 pricing.json 时字段映射错位
        // （例如把 cache_creation 填进了 cache_read），会让缓存读被放大十几倍。
        for (name, p) in &table().models {
            assert!(p.input > 0.0 && p.output > 0.0, "{name}");
            assert!(p.cache_read <= p.input, "{name}: 缓存读不应贵于输入");
            if p.cache_write_1h > 0.0 {
                assert!(
                    p.cache_write_1h >= p.cache_write_5m,
                    "{name}: 1h 写应 ≥ 5m 写"
                );
            }
            if let Some(lc) = p.long_context {
                assert!(lc.above_tokens >= 100_000, "{name}: 阈值单位应为 token");
                assert!(lc.input >= p.input && lc.output >= p.output, "{name}");
            }
        }
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
