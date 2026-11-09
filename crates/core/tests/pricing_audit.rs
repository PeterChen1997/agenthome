//! 成本口径回归测试（审计记录见 docs/12-成本口径核对.md）。
//!
//! 每条都对应一个「算错了会差多少」的事实，数值取自 2026-11-09 的 LiteLLM 快照，
//! 与 ccusage 20.0.48 的算法逐项对齐过。

use agenthome_core::model::Usage;
use agenthome_core::pricing::{estimate_usd, lookup};

const M: u64 = 1_000_000;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// 本机 Claude 用量里缓存读约占 token 的 97%。缓存读必须按 cache_read 单价（输入的 0.05~0.1×）计，
/// 按输入价计会把总成本放大约 9 倍（本机实测 $19.5k → $179k）。
#[test]
fn 缓存读按缓存单价计而不是输入价() {
    // 单次请求 20 万缓存读：低于 OpenAI 的 272k 长上下文阈值，只看缓存单价本身。
    let u = Usage {
        cache_read_tokens: 200_000,
        ..Usage::default()
    };
    let opus = lookup("claude-opus-4-8").unwrap();
    assert!(
        close(opus.cache_read, opus.input * 0.1),
        "Anthropic 缓存读 = 0.1× 输入"
    );
    assert!(close(
        estimate_usd("claude-opus-4-8", &u).unwrap(),
        0.2 * 0.5
    ));
    assert!(close(estimate_usd("gpt-5.5", &u).unwrap(), 0.2 * 0.5));
}

/// Claude Code 主会话写的是 1 小时缓存（2×输入），子代理写 5 分钟缓存（1.25×）。
/// 全按 5m 计会把本机 Claude 成本低估约 8%（$19.5k → $17.9k）；没拆分的旧记录只能按 5m。
#[test]
fn 缓存写区分五分钟与一小时() {
    let split = Usage {
        cache_write_tokens: 2 * M,
        cache_write_5m_tokens: M,
        cache_write_1h_tokens: M,
        ..Usage::default()
    };
    // opus-4-8: 5m 6.25 + 1h 10
    assert!(close(
        estimate_usd("claude-opus-4-8", &split).unwrap(),
        6.25 + 10.0
    ));
    let unsplit = Usage {
        cache_write_tokens: 2 * M,
        ..Usage::default()
    };
    assert!(close(
        estimate_usd("claude-opus-4-8", &unsplit).unwrap(),
        2.0 * 6.25
    ));
}

/// gpt-5.4 及之后的 OpenAI 模型：单次请求提示 > 272k 时整次请求（含输出）按高价档。
/// 本机 Codex 有 2,859 次请求越过阈值，漏算这档会把 Codex 成本低估约 7%（$3,259 → $3,483）。
#[test]
fn 长上下文高价档按单次请求判断() {
    let below = Usage {
        input_tokens: 72_000,
        cache_read_tokens: 200_000,
        output_tokens: 10_000,
        ..Usage::default()
    };
    let above = Usage {
        input_tokens: 72_001,
        ..below
    };
    // gpt-6.1-sol 基础 2 / 10 / 0.1，> 272k 为 4 / 15 / 0.2
    let base = 0.072 * 2.0 + 0.01 * 10.0 + 0.2 * 0.1;
    let high = 0.072_001 * 4.0 + 0.01 * 15.0 + 0.2 * 0.2;
    assert!(
        close(estimate_usd("gpt-6.1-sol", &below).unwrap(), base),
        "恰好 272k 不进高价档"
    );
    assert!(close(estimate_usd("gpt-6.1-sol", &above).unwrap(), high));

    // 正因为按单次请求判断，估算必须逐请求做再求和：把两次小请求合并后再估会被误判成高价档。
    let mut merged = below;
    merged.add(&below);
    let per_request = 2.0 * estimate_usd("gpt-6.1-sol", &below).unwrap();
    assert!(estimate_usd("gpt-6.1-sol", &merged).unwrap() > per_request);

    // 没有长上下文档的模型不受影响。
    let big = Usage {
        cache_read_tokens: 900_000,
        ..Usage::default()
    };
    assert!(close(
        estimate_usd("claude-opus-5-5", &big).unwrap(),
        0.9 * 0.2
    ));
}

/// Anthropic 的 haiku-5-5 在 LiteLLM 里有 >100k 档，缓存读也一起涨价。
#[test]
fn claude_长上下文档同样覆盖缓存() {
    let u = Usage {
        cache_read_tokens: 150_000,
        output_tokens: 1_000,
        ..Usage::default()
    };
    let c = estimate_usd("claude-haiku-5-5", &u).unwrap();
    assert!(close(c, 0.15 * 0.05 + 0.001 * 2.5), "{c}");
}

/// OpenAI 的 input_tokens 含 cached_input_tokens。原始口径（input_includes_cache = true）要先扣掉缓存，
/// 否则缓存部分按全价再算一遍；reasoning_output_tokens 已含在 output_tokens 里，不能再加。
#[test]
fn openai_缓存与推理不重复计() {
    let raw = Usage {
        input_tokens: 200_000,
        cache_read_tokens: 180_000,
        output_tokens: 20_000,
        reasoning_tokens: 12_000,
        input_includes_cache: true,
        ..Usage::default()
    };
    let normalized = Usage {
        input_tokens: 20_000,
        input_includes_cache: false,
        ..raw
    };
    // gpt-5.5: 0.02M*5 + 0.18M*0.5 + 0.02M*30（推理不另计）
    let want = 0.1 + 0.09 + 0.6;
    assert!(close(estimate_usd("gpt-5.5", &raw).unwrap(), want));
    assert!(close(estimate_usd("gpt-5.5", &normalized).unwrap(), want));
}

/// 本机实际出现过的模型都必须能定价；未知模型返回 None 而不是按 0 计。
#[test]
fn 本机出现的模型全部可定价() {
    for m in [
        "claude-opus-5-5",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-fable-5",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "gpt-6-astra",
        "gpt-5.5",
        "gpt-6.1-sol",
        "gpt-5.4",
        "gpt-5.6-sol",
        "gpt-6-sol",
        "gpt-5.3-codex",
        "gpt-5.3-codex-spark",
        "gpt-5.2",
        "gpt-5.4-mini",
    ] {
        assert!(lookup(m).is_some(), "{m}");
    }
    assert_eq!(
        estimate_usd("gemini-3-pro-preview", &Usage::default()),
        None
    );
}
