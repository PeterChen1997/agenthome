//! 按机器统计（CARM-284 方案单元 11）：机器 × 来源 × 供应商 × 模型 的用量与等价花费。
//!
//! 口径统一按**单次请求**：本机和远端都读协议 v1 用量记录（本机是采集端用量库 `usage_store`，
//! 远端是 Mac 导入库 `usage_import`），两边是同一条投影代码产出的，逐条调用 `pricing::estimate_usd`
//! 后再求和。长上下文跨档在单次请求上判断，所以同一份用量放在哪台机器上算都一样贵。
//!
//! 不用索引库（`store.rs`）算本机：它把 Codex 一轮里的多次请求合并成一条消息，合并后的提示可能越过
//! 长上下文阈值，整条按高价计（CARM-290 本机快照偏高约 2%）；OpenCode 也不在索引里。
//!
//! 合并只发生一次：本机记录 + 远端记录拼成一个列表后统一分组。远端里不可能有本机
//! （导入库拒绝把本机配对为远端），这里再校验一遍，避免同一份用量被算两次。
//!
//! 缺价：定价表里没有的模型照样计 token，另记未知价请求数与 token；整组都没有价时 `cost_usd` 为空。

use crate::cost::Vendor;
use crate::model::{CostBreakdown, ProviderId, Usage};
use crate::usage_import::ImportStore;
use crate::usage_store::UsageStore;
use crate::usage_v1::UsageRecord;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// 统计范围 `[from_ms, to_ms)`，UTC 毫秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub from_ms: i64,
    pub to_ms: i64,
}

impl Window {
    pub fn new(from_ms: i64, to_ms: i64) -> Result<Self> {
        ensure!(from_ms < to_ms, "起始时间必须早于结束时间");
        Ok(Self { from_ms, to_ms })
    }
}

/// 机器条件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MachineFilter {
    /// 本机 + 全部远端。
    All,
    Local,
    /// 一台远端，按 `machine_id`。
    Remote(String),
}

/// 机器在本机和远端里的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Local,
    Remote,
}

/// 一组请求的合计。
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    /// 用量记录条数。
    pub events: u64,
    pub requests: u64,
    /// 新鲜输入（不含缓存读）。
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    /// 输出的子项，不另计进 `total_tokens`。
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// 已计价请求的等价花费之和；这组一条有价的都没有时为空（「未定价」，不是 0 元）。
    pub cost_usd: Option<f64>,
    /// 定价表里没有价的请求数和它们的 token。
    pub unpriced_requests: u64,
    pub unpriced_tokens: u64,
}

impl Tally {
    fn add(&mut self, r: &UsageRecord) {
        let tokens = r.input_tokens
            + r.output_tokens
            + r.cache_read_tokens
            + r.cache_write_5m_tokens
            + r.cache_write_1h_tokens;
        self.events += 1;
        self.requests += u64::from(r.requests);
        self.input_tokens += r.input_tokens;
        self.output_tokens += r.output_tokens;
        self.cache_read_tokens += r.cache_read_tokens;
        self.cache_write_5m_tokens += r.cache_write_5m_tokens;
        self.cache_write_1h_tokens += r.cache_write_1h_tokens;
        self.reasoning_tokens += r.reasoning_tokens;
        self.total_tokens += tokens;
        match request_cost(r) {
            Some(c) => self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c),
            None => {
                self.unpriced_requests += u64::from(r.requests);
                self.unpriced_tokens += tokens;
            }
        }
    }

    fn merge(&mut self, o: &Tally) {
        self.events += o.events;
        self.requests += o.requests;
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_5m_tokens += o.cache_write_5m_tokens;
        self.cache_write_1h_tokens += o.cache_write_1h_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
        self.total_tokens += o.total_tokens;
        self.cost_usd = match (self.cost_usd, o.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        self.unpriced_requests += o.unpriced_requests;
        self.unpriced_tokens += o.unpriced_tokens;
    }
}

/// 一行：一台机器上一个来源跑一个模型的合计。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReportRow {
    pub machine_id: String,
    /// 本机为 `local`，远端为配对时起的连接名。
    pub machine: String,
    pub kind: MachineKind,
    /// 采集来源（claude-code / codex / gemini-cli / antigravity-cli / opencode）。
    pub source: String,
    /// 模型厂商；说不清为 `unknown`。
    pub vendor: String,
    /// 请求记录的实际模型；缺失为 `unknown`。
    pub model: String,
    #[serde(flatten)]
    pub tally: Tally,
}

impl ReportRow {
    /// 换成首页花费卡用的明细行（CARM-301）：首页和 CLI 对账表读同一份逐请求合计，金额只算一次。
    /// `unknown` 模型留空，交给 [`crate::cost::CostSummary`] 统一显示为 unknown。
    pub fn cost_breakdown(&self) -> CostBreakdown {
        let t = &self.tally;
        CostBreakdown {
            key: if self.model == "unknown" {
                String::new()
            } else {
                self.model.clone()
            },
            provider: ProviderId::parse(&self.source),
            usage: Usage {
                input_tokens: t.input_tokens,
                output_tokens: t.output_tokens,
                cache_read_tokens: t.cache_read_tokens,
                cache_write_tokens: t.cache_write_5m_tokens + t.cache_write_1h_tokens,
                cache_write_5m_tokens: t.cache_write_5m_tokens,
                cache_write_1h_tokens: t.cache_write_1h_tokens,
                reasoning_tokens: t.reasoning_tokens,
                requests: t.requests,
                input_includes_cache: false,
            },
            cost_usd: t.cost_usd.unwrap_or(0.0),
            unpriced_tokens: t.unpriced_tokens,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub pricing_version: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// 按 本机在前 → 连接名 → 来源 → 供应商 → 模型 排序。
    pub rows: Vec<ReportRow>,
    pub totals: Tally,
}

/// 单次请求的等价花费；模型不在定价表里为 `None`。
fn request_cost(r: &UsageRecord) -> Option<f64> {
    let usage = Usage {
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cache_read_tokens: r.cache_read_tokens,
        cache_write_tokens: r.cache_write_5m_tokens + r.cache_write_1h_tokens,
        cache_write_5m_tokens: r.cache_write_5m_tokens,
        cache_write_1h_tokens: r.cache_write_1h_tokens,
        reasoning_tokens: r.reasoning_tokens,
        requests: u64::from(r.requests),
        input_includes_cache: false,
    };
    crate::pricing::estimate_usd(r.model.as_deref()?, &usage)
}

/// 记录带了厂商就用它；没带（采集端认不出）按模型名再认一次，仍认不出记 `unknown`。
fn vendor_of(r: &UsageRecord) -> String {
    if let Some(v) = &r.vendor {
        return v.clone();
    }
    match Vendor::of(
        r.model.as_deref().unwrap_or(""),
        &ProviderId::parse(&r.source),
    ) {
        Vendor::Other => "unknown".to_string(),
        v => v.as_str().to_string(),
    }
}

/// 按机器条件查询并分组。`local` 为空表示本机没有用量库（例如还没跑过采集）：
/// 只看本机时报错，看全部时只算远端。远端的连接名取自导入库的配对表。
pub fn query(
    local: Option<&UsageStore>,
    remote: Option<&ImportStore>,
    filter: &MachineFilter,
    window: Window,
) -> Result<Report> {
    let mut records = Vec::new();
    let mut labels = HashMap::new();
    let local_id = local.map(|s| s.machine_id().to_string());
    if matches!(filter, MachineFilter::All | MachineFilter::Local) {
        match local {
            Some(store) => records.extend(store.records_in(window.from_ms, window.to_ms)?),
            None => ensure!(
                *filter == MachineFilter::All,
                "本机还没有用量库，先运行一次采集"
            ),
        }
    }
    let remote_machine = match filter {
        MachineFilter::All => Some(None),
        MachineFilter::Local => None,
        MachineFilter::Remote(id) => Some(Some(id.as_str())),
    };
    if let Some(machine) = remote_machine {
        match remote {
            Some(store) => {
                labels.extend(store.peers()?);
                if let Some(id) = machine {
                    ensure!(labels.contains_key(id), "这台远端机器没有配对过");
                }
                let rows = store.records_in(window.from_ms, window.to_ms, machine)?;
                ensure!(
                    rows.iter()
                        .all(|r| Some(&r.machine_id) != local_id.as_ref()),
                    "远端用量里出现了本机，拒绝重复计入"
                );
                records.extend(rows);
            }
            None => ensure!(
                machine.is_none(),
                "还没有远端用量库，这台远端机器没有配对过"
            ),
        }
    }
    Ok(aggregate(&records, local_id.as_deref(), &labels, window))
}

/// 把一批记录（本机与远端已拼好）分组合计。只在这里合并一次。
pub fn aggregate(
    records: &[UsageRecord],
    local_machine_id: Option<&str>,
    labels: &HashMap<String, String>,
    window: Window,
) -> Report {
    type Key = (MachineKind, String, String, String, String, String);
    let mut groups: BTreeMap<Key, Tally> = BTreeMap::new();
    for r in records {
        let (kind, machine) = if Some(r.machine_id.as_str()) == local_machine_id {
            (MachineKind::Local, "local".to_string())
        } else {
            let label = labels
                .get(&r.machine_id)
                .cloned()
                .unwrap_or_else(|| r.machine_id.clone());
            (MachineKind::Remote, label)
        };
        let key = (
            kind,
            machine,
            r.machine_id.clone(),
            r.source.clone(),
            vendor_of(r),
            r.model.clone().unwrap_or_else(|| "unknown".to_string()),
        );
        groups.entry(key).or_default().add(r);
    }
    let mut totals = Tally::default();
    let rows = groups
        .into_iter()
        .map(
            |((kind, machine, machine_id, source, vendor, model), tally)| {
                totals.merge(&tally);
                ReportRow {
                    machine_id,
                    machine,
                    kind,
                    source,
                    vendor,
                    model,
                    tally,
                }
            },
        )
        .collect();
    Report {
        pricing_version: crate::pricing::pricing_version().to_string(),
        from_ms: window.from_ms,
        to_ms: window.to_ms,
        rows,
        totals,
    }
}
