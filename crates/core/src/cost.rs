//! 首页「花费」卡的口径（CARM-283）：按模型厂商汇总、模型排行、未计价单列、订阅月费折算。
//!
//! - **供应商按模型厂商分**，不按来源分：Antigravity 也能跑 Claude / GPT，按来源分会把厂商混在一起。
//!   来源（Claude Code / Codex / Antigravity …）只作为副标签。
//! - **等价 API 花费**：消息按定价表估算之和（`messages.cost_usd`），不是账单。
//! - **未计价**：定价表里没有的模型不按 0 计，token 单独列出，界面要标出来。
//! - **实付**：用户在设置里填的订阅月费按所选天数折算（30 天 = 1 个月），不读账单、不读凭据。

use crate::model::{CostBreakdown, ProviderId};

/// 模型厂商。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Vendor {
    Anthropic,
    OpenAI,
    Google,
    Other,
}

impl Vendor {
    pub const ALL: [Vendor; 4] = [
        Vendor::Anthropic,
        Vendor::OpenAI,
        Vendor::Google,
        Vendor::Other,
    ];

    /// 埋点、配置键用的稳定名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::OpenAI => "openai",
            Vendor::Google => "google",
            Vendor::Other => "other",
        }
    }

    /// 先看模型名前缀；认不出（空名、`<synthetic>` 等）再按来源兜底。
    /// Antigravity 认不出时归 Other，因为它同时跑 Gemini、Claude 和 GPT。
    pub fn of(model: &str, provider: &ProviderId) -> Vendor {
        let m = crate::pricing::normalize_model(model);
        if m.starts_with("claude-") {
            return Vendor::Anthropic;
        }
        if m.starts_with("gpt-")
            || m.starts_with("codex-")
            || m.starts_with("chatgpt-")
            || (m.starts_with('o') && m[1..].starts_with(|c: char| c.is_ascii_digit()))
        {
            return Vendor::OpenAI;
        }
        if m.starts_with("gemini-") || m.starts_with("gemma-") {
            return Vendor::Google;
        }
        match provider {
            ProviderId::ClaudeCode => Vendor::Anthropic,
            ProviderId::Codex => Vendor::OpenAI,
            ProviderId::GeminiCli => Vendor::Google,
            _ => Vendor::Other,
        }
    }
}

/// 一个厂商在所选范围内的汇总。
#[derive(Clone, Debug, PartialEq)]
pub struct VendorCost {
    pub vendor: Vendor,
    /// 等价 API 花费（只含已计价部分）。
    pub cost_usd: f64,
    /// 全部 token（含未计价）。
    pub tokens: u64,
    pub unpriced_tokens: u64,
    /// 各来源的 token，按 token 倒序。
    pub sources: Vec<(ProviderId, u64)>,
}

/// 一个模型在所选范围内的汇总（多个来源跑同一模型时合并成一行）。
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCost {
    pub model: String,
    pub vendor: Vendor,
    pub cost_usd: f64,
    pub tokens: u64,
    pub unpriced_tokens: u64,
    pub sources: Vec<ProviderId>,
}

impl ModelCost {
    /// 整个模型都没有价（定价表里没有）。只有部分消息没价时仍按已计价排名，另标 `unpriced_tokens`。
    pub fn is_unpriced(&self) -> bool {
        self.unpriced_tokens > 0 && self.unpriced_tokens >= self.tokens
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostSummary {
    /// 等价 API 花费合计（只含已计价部分）。
    pub total_usd: f64,
    pub unpriced_tokens: u64,
    /// 有用量的厂商，按花费倒序（同额按 token 倒序）。
    pub vendors: Vec<VendorCost>,
    /// 已计价的模型在前、按花费倒序；整个未计价的模型排在最后、按 token 倒序。
    pub models: Vec<ModelCost>,
}

impl CostSummary {
    /// 由 [`crate::store::Store::stats_by_model`] 的行汇总。
    pub fn from_breakdown(rows: &[CostBreakdown]) -> CostSummary {
        let mut models: Vec<ModelCost> = Vec::new();
        for row in rows {
            let tokens = row.usage.input_tokens
                + row.usage.output_tokens
                + row.usage.cache_read_tokens
                + row.usage.cache_write_tokens;
            if tokens == 0 && row.cost_usd == 0.0 {
                continue;
            }
            let vendor = Vendor::of(&row.key, &row.provider);
            let model = if row.key.is_empty() {
                "unknown".to_string()
            } else {
                row.key.clone()
            };
            let entry = match models
                .iter_mut()
                .position(|m| m.model == model && m.vendor == vendor)
            {
                Some(i) => &mut models[i],
                None => {
                    models.push(ModelCost {
                        model,
                        vendor,
                        cost_usd: 0.0,
                        tokens: 0,
                        unpriced_tokens: 0,
                        sources: Vec::new(),
                    });
                    models.last_mut().expect("刚 push 过")
                }
            };
            entry.cost_usd += row.cost_usd;
            entry.tokens += tokens;
            entry.unpriced_tokens += row.unpriced_tokens.min(tokens);
            if !entry.sources.contains(&row.provider) {
                entry.sources.push(row.provider.clone());
            }
        }
        models.sort_by(|a, b| {
            a.is_unpriced()
                .cmp(&b.is_unpriced())
                .then(b.cost_usd.total_cmp(&a.cost_usd))
                .then(b.tokens.cmp(&a.tokens))
                .then(a.model.cmp(&b.model))
        });

        let mut vendors: Vec<VendorCost> = Vec::new();
        for row in rows {
            let tokens = row.usage.input_tokens
                + row.usage.output_tokens
                + row.usage.cache_read_tokens
                + row.usage.cache_write_tokens;
            if tokens == 0 && row.cost_usd == 0.0 {
                continue;
            }
            let vendor = Vendor::of(&row.key, &row.provider);
            let entry = match vendors.iter_mut().position(|v| v.vendor == vendor) {
                Some(i) => &mut vendors[i],
                None => {
                    vendors.push(VendorCost {
                        vendor,
                        cost_usd: 0.0,
                        tokens: 0,
                        unpriced_tokens: 0,
                        sources: Vec::new(),
                    });
                    vendors.last_mut().expect("刚 push 过")
                }
            };
            entry.cost_usd += row.cost_usd;
            entry.tokens += tokens;
            entry.unpriced_tokens += row.unpriced_tokens.min(tokens);
            match entry.sources.iter_mut().find(|(p, _)| *p == row.provider) {
                Some((_, n)) => *n += tokens,
                None => entry.sources.push((row.provider.clone(), tokens)),
            }
        }
        for v in &mut vendors {
            v.sources.sort_by_key(|a| std::cmp::Reverse(a.1));
        }
        vendors.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then(b.tokens.cmp(&a.tokens))
                .then(a.vendor.cmp(&b.vendor))
        });

        CostSummary {
            total_usd: vendors.iter().map(|v| v.cost_usd).sum(),
            unpriced_tokens: vendors.iter().map(|v| v.unpriced_tokens).sum(),
            vendors,
            models,
        }
    }

    /// 整个没有价的模型（界面单独标出、Sentry 上报用）。
    pub fn unpriced_models(&self) -> impl Iterator<Item = &ModelCost> {
        self.models.iter().filter(|m| m.is_unpriced())
    }
}

/// 订阅月费按天数折算成所选范围的实付：`月费 × 天数 / 30`。
/// 「今天」按 1 天、「7 天」按 7 天、「30 天」正好一个月。
pub fn prorated_fee(monthly_usd: f64, days: u32) -> f64 {
    monthly_usd * f64::from(days) / 30.0
}

/// 有意不计价的模型：没有公开 API 单价，不能借别的模型的价猜。
/// 它们照样在界面上标「未计价」，但不当成定价表缺口上报。
pub fn has_no_public_price(model: &str) -> bool {
    matches!(
        crate::pricing::normalize_model(model).as_str(),
        // Codex 内部的自动审批审查模型（models_cache 里 visibility=hide）。
        "codex-auto-review"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;

    fn row(
        key: &str,
        provider: ProviderId,
        tokens: u64,
        cost: f64,
        unpriced: u64,
    ) -> CostBreakdown {
        CostBreakdown {
            key: key.to_string(),
            provider,
            usage: Usage {
                cache_read_tokens: tokens,
                ..Usage::default()
            },
            cost_usd: cost,
            unpriced_tokens: unpriced,
        }
    }

    #[test]
    fn 供应商按模型厂商分_不按来源() {
        // Antigravity 跑的 Claude 要算进 Anthropic，否则用户看到的 Claude 花费偏低、Gemini 偏高。
        assert_eq!(
            Vendor::of("claude-opus-5-5", &ProviderId::AntigravityCli),
            Vendor::Anthropic
        );
        assert_eq!(
            Vendor::of("gpt-6.2-sol", &ProviderId::AntigravityCli),
            Vendor::OpenAI
        );
        assert_eq!(
            Vendor::of("gemini-4-hydrogen", &ProviderId::AntigravityCli),
            Vendor::Google
        );
        assert_eq!(
            Vendor::of("codex-auto-review", &ProviderId::Codex),
            Vendor::OpenAI
        );
        assert_eq!(Vendor::of("o4-mini", &ProviderId::Codex), Vendor::OpenAI);
        assert_eq!(
            Vendor::of(
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                &ProviderId::Other("x".into())
            ),
            Vendor::Anthropic
        );
        // 认不出的名字按来源兜底；Antigravity 是多厂商来源，兜底为 Other 而不是猜 Google。
        assert_eq!(Vendor::of("", &ProviderId::ClaudeCode), Vendor::Anthropic);
        assert_eq!(Vendor::of("", &ProviderId::AntigravityCli), Vendor::Other);
        assert_eq!(Vendor::of("opus", &ProviderId::Codex), Vendor::OpenAI);
    }

    #[test]
    fn 未计价模型不按零元混进排行_单独排在最后() {
        let s = CostSummary::from_breakdown(&[
            row("claude-opus-5-5", ProviderId::ClaudeCode, 1_000, 50.0, 0),
            row(
                "gemini-4-hydrogen",
                ProviderId::AntigravityCli,
                5_000,
                0.0,
                5_000,
            ),
            row("gpt-6.1-sol", ProviderId::Codex, 800, 5.0, 0),
        ]);
        let names: Vec<&str> = s.models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(
            names,
            ["claude-opus-5-5", "gpt-6.1-sol", "gemini-4-hydrogen"]
        );
        assert!(s.models[2].is_unpriced());
        assert_eq!(s.unpriced_models().count(), 1);
        assert_eq!(s.unpriced_tokens, 5_000);
        // 合计只含已计价部分，不能把未计价当 0 元「算进去」后看起来完整。
        assert!((s.total_usd - 55.0).abs() < 1e-9);
        let google = s
            .vendors
            .iter()
            .find(|v| v.vendor == Vendor::Google)
            .unwrap();
        assert_eq!(google.unpriced_tokens, 5_000);
    }

    #[test]
    fn 同一模型多个来源合并成一行_厂商合计含各来源() {
        let s = CostSummary::from_breakdown(&[
            row("claude-opus-5-5", ProviderId::ClaudeCode, 1_000, 40.0, 0),
            row("claude-opus-5-5", ProviderId::AntigravityCli, 300, 12.0, 0),
            row("gemini-4-neon", ProviderId::AntigravityCli, 200, 3.0, 0),
        ]);
        let opus: Vec<_> = s
            .models
            .iter()
            .filter(|m| m.model == "claude-opus-5-5")
            .collect();
        assert_eq!(opus.len(), 1);
        assert!((opus[0].cost_usd - 52.0).abs() < 1e-9);
        assert_eq!(opus[0].tokens, 1_300);
        assert_eq!(
            opus[0].sources,
            [ProviderId::ClaudeCode, ProviderId::AntigravityCli]
        );
        let anthropic = s.vendors.first().unwrap();
        assert_eq!(anthropic.vendor, Vendor::Anthropic);
        assert_eq!(
            anthropic.sources,
            [
                (ProviderId::ClaudeCode, 1_000),
                (ProviderId::AntigravityCli, 300)
            ]
        );
        // 厂商合计之和 = 模型合计之和 = 总额：首页三处数字必须对得上。
        let by_model: f64 = s.models.iter().map(|m| m.cost_usd).sum();
        assert!((by_model - s.total_usd).abs() < 1e-9);
    }

    #[test]
    fn 部分消息未计价的模型仍按已计价金额排名并标出缺口() {
        let s =
            CostSummary::from_breakdown(&[row("gpt-6.2-sol", ProviderId::Codex, 1_000, 2.0, 400)]);
        assert!(!s.models[0].is_unpriced());
        assert_eq!(s.models[0].unpriced_tokens, 400);
    }

    #[test]
    fn 月费按天数折算() {
        assert!((prorated_fee(200.0, 30) - 200.0).abs() < 1e-9);
        assert!((prorated_fee(200.0, 7) - 200.0 * 7.0 / 30.0).abs() < 1e-9);
        assert!((prorated_fee(30.0, 1) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn 只有内部审查模型算作没有公开价() {
        assert!(has_no_public_price("codex-auto-review"));
        assert!(!has_no_public_price("gemini-4-hydrogen"));
    }
}
