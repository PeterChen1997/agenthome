//! 成本口径回归测试（审计记录见 docs/12-成本口径核对.md）。
//!
//! 每条都对应一个「算错了会差多少」的事实，来源快照与历史对账见成本口径文档。

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
        // Gemini CLI 会话里出现过的模型（LiteLLM 只有 vertex_ai/ 前缀的条目，生成价表时去掉了前缀）。
        "gemini-3-pro-preview",
        "gemini-2.5-pro",
        // CARM-283 补的：Antigravity 记下的 Gemini 4 系列，以及 Claude Fable 5.2、GPT-6.2。
        // 缺任何一个，那个模型的全部用量都会在首页消失（2026-11-19 前 Hydrogen 2,287 次请求全是未计价）。
        "gemini-4-hydrogen",
        "gemini-4-neon",
        "gemini-4-carbon",
        "claude-fable-5-2",
        "gpt-6.2-sol",
        "gpt-6.2-astra",
    ] {
        assert!(lookup(m).is_some(), "{m}");
    }
    // 不认识的模型必须是 None（界面显示「未知」），不能悄悄按 0 计成「免费」。
    assert_eq!(
        estimate_usd("some-unknown-model-x", &Usage::default()),
        None
    );
}

/// Antigravity 和 Multica 配置里的模型名带推理档位后缀（`gemini-4-hydrogen-xhigh`、
/// `gemini-4-hydrogen-tiered-high`、`gemini-4-neon-xhigh`）。档位不改单价，必须落到基础型号的价；
/// 落不到就整段用量变成「未计价」，Gemini 花费又会回到 $0。
#[test]
fn gemini_档位后缀落到基础型号价() {
    for (name, base) in [
        ("gemini-4-hydrogen-xhigh", "gemini-4-hydrogen"),
        ("gemini-4-hydrogen-tiered-high", "gemini-4-hydrogen"),
        ("gemini-4-hydrogen-tiered-xhigh", "gemini-4-hydrogen"),
        ("gemini-4-neon-xhigh", "gemini-4-neon"),
    ] {
        assert_eq!(lookup(name), lookup(base), "{name}");
        assert!(lookup(name).is_some(), "{name}");
    }
    // Hydrogen 与 Neon 不同价（1.5 vs 3.0 /M 输入）：名字归错型号会差一倍。
    assert!(lookup("gemini-4-neon").unwrap().input > lookup("gemini-4-hydrogen").unwrap().input);
}

/// Gemini 4 的缓存读按 cache_read 单价（输入的 0.05×）。Antigravity 会话里缓存读占 token 的 96%，
/// 按输入价算会把 Gemini 花费放大约 15 倍。
#[test]
fn gemini_4_缓存读按缓存单价计() {
    let u = Usage {
        input_tokens: 100_000,
        cache_read_tokens: 1_000_000,
        output_tokens: 10_000,
        ..Usage::default()
    };
    // hydrogen: 0.1M*1.5 + 1M*0.075 + 0.01M*7.5
    assert!(close(
        estimate_usd("gemini-4-hydrogen-xhigh", &u).unwrap(),
        0.15 + 0.075 + 0.075
    ));
}

/// 漏掉价表中的付费模型会让整组请求排除在首页与 CLI 金额之外。
/// Runner 中这三个模型之前共有 1,072 次未计价请求，缓存读占用量大头。
#[test]
fn runner_付费模型单价与_litellm_快照一致() {
    // 2026-11-26 LiteLLM 快照：input / output / cache_read / cache_write_5m / cache_write_1h。
    for (model, rates) in [
        // opencode_go/deepseek-v4.1-flash
        ("deepseek-v4.1-flash", [0.3, 1.2, 0.006, 0.0, 0.0]),
        // moonshot/kimi-k3.1-pro
        ("kimi-k3.1-pro", [2.0, 10.0, 0.2, 2.0, 4.0]),
        // dashscope/qwen4-max
        ("qwen4-max", [1.5, 4.5, 0.075, 1.875, 3.0]),
    ] {
        let price = lookup(model).unwrap_or_else(|| panic!("{model} 必须可计价"));
        assert_eq!(
            [
                price.input,
                price.output,
                price.cache_read,
                price.cache_write_5m,
                price.cache_write_1h,
            ],
            rates,
            "{model}"
        );
        let usage = Usage {
            input_tokens: 100_000,
            output_tokens: 20_000,
            cache_read_tokens: 50_000,
            ..Usage::default()
        };
        assert!(close(
            estimate_usd(model, &usage).unwrap(),
            0.1 * rates[0] + 0.02 * rates[1] + 0.05 * rates[2]
        ));
    }
}

/// models.dev 的 OpenCode 免费档明确为 $0。漏价会让免费用量显示成「未计价」，
/// 剥掉 -free 则会误用付费单价。
#[test]
fn runner_免费档已计价且成本为零() {
    let usage = Usage {
        input_tokens: 100_000,
        output_tokens: 20_000,
        cache_read_tokens: 50_000,
        cache_write_5m_tokens: 10_000,
        cache_write_1h_tokens: 10_000,
        ..Usage::default()
    };
    assert_eq!(estimate_usd("deepseek-v4.1-flash-free", &usage), Some(0.0));
    assert_eq!(lookup("deepseek-v4.1-flash-other-free"), None);
}

/// `codex-auto-review` 是 Codex 内部的自动审批审查模型（models_cache 里 visibility=hide），
/// 没有公开 API 单价。它必须保持「未计价」并在界面单独标出，不能借用某个 GPT 的价猜一个数。
#[test]
fn codex_内部审查模型没有公开价_保持未计价() {
    assert_eq!(lookup("codex-auto-review"), None);
}
