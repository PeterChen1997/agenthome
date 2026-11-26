//! 远程用量同步协议 v1：一条记录 = 一次计费请求的用量（CARM-284 方案「同步单位与统计口径」）。
//!
//! 只有白名单字段：机器、来源、模型供应商、模型、UTC 毫秒、请求数和各类 token；协议字段只有
//! 版本、不透明事件键、修订序号、操作类型。正文、标题、路径、工具参数、账号都不在结构里，
//! 解码一律 `deny_unknown_fields`，多一个字段就整页拒收。
//!
//! 去重键是 `(machine_id, source, event_key)`；同键较新的 `revision` 整行替换旧的。

use crate::device::{DeviceIdentity, hex, is_uuid};
use crate::model::ProviderId;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;
/// 一页序列化后的上限，超过整页拒收（采集端按它切页）。
pub const MAX_PAGE_BYTES: usize = 256 * 1024;
/// 模型、供应商名的长度上限。
const MAX_NAME_LEN: usize = 128;

/// 操作类型。`Retract`：撤回此前发过的一条（已确认的解析更正），用量字段必须为 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Upsert,
    Retract,
}

/// 一条用量记录。token 口径与 `model::Usage` 相同：`input_tokens` 是新鲜输入（不含缓存读），
/// 缓存写分 5 分钟 / 1 小时两档，`reasoning_tokens` 是 `output_tokens` 的子项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRecord {
    pub v: u32,
    pub event_key: String,
    /// 同一事件的修订序号，从 1 起；导入端只接受比已有更大的。
    pub revision: u64,
    pub op: Op,
    pub machine_id: String,
    /// 采集来源，`ProviderId::as_str`（claude-code / codex / gemini-cli / antigravity-cli / opencode）。
    pub source: String,
    /// 模型厂商（anthropic / openai / google ...）；来源说不清时为空，导入端按模型归类或记「未知」。
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// 请求时间，UTC 毫秒。
    pub ts_ms: i64,
    pub requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub reasoning_tokens: u64,
}

/// 一页记录，全部来自 `machine_id` 这台机器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsagePage {
    pub v: u32,
    pub machine_id: String,
    pub records: Vec<UsageRecord>,
}

/// 事件键的依据。优先用来源自带的稳定请求标识。
#[derive(Clone, Copy, Debug)]
pub enum EventBasis<'a> {
    /// 来源的请求标识：Claude 的 `message.id`、Codex 的 response id 等。
    /// Claude fork 复制过来的同一次响应 id 相同，只计一次。
    Request(&'a str),
    /// 没有请求标识的旧格式：会话原生 id + 该会话内稳定的用量记录序号。
    Ordinal {
        native_session_id: &'a str,
        ordinal: u64,
    },
}

/// 不透明事件键：HMAC-SHA256(本机密钥, 版本 + 来源 + 依据)，64 位 hex。
/// 不含主机名和路径，改主机名、移动文件都不变；没有密钥的人不能从键反推请求 id。
pub fn event_key(identity: &DeviceIdentity, source: &ProviderId, basis: EventBasis<'_>) -> String {
    let mut msg = Vec::new();
    for part in [b"agenthome-usage-v1".as_slice(), source.as_str().as_bytes()] {
        push_part(&mut msg, part);
    }
    match basis {
        EventBasis::Request(id) => {
            push_part(&mut msg, b"request");
            push_part(&mut msg, id.as_bytes());
        }
        EventBasis::Ordinal {
            native_session_id,
            ordinal,
        } => {
            push_part(&mut msg, b"ordinal");
            push_part(&mut msg, native_session_id.as_bytes());
            push_part(&mut msg, &ordinal.to_be_bytes());
        }
    }
    hex(&hmac_sha256(identity.secret(), &msg))
}

/// 长度前缀拼接，避免 `"a" + "bc"` 和 `"ab" + "c"` 撞键。
fn push_part(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u64).to_be_bytes());
    out.extend_from_slice(part);
}

fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

impl UsageRecord {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.v == VERSION, "不支持的版本 {}", self.v);
        ensure!(
            self.event_key.len() == 64
                && self
                    .event_key
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "event_key 不是 64 位小写 hex"
        );
        ensure!(self.revision >= 1, "revision 从 1 起");
        ensure!(is_uuid(&self.machine_id), "machine_id 不是 UUID");
        ensure!(
            matches!(
                ProviderId::parse(&self.source),
                ProviderId::ClaudeCode
                    | ProviderId::Codex
                    | ProviderId::GeminiCli
                    | ProviderId::AntigravityCli
                    | ProviderId::OpenCode
            ),
            "不认识的来源 {:?}",
            self.source
        );
        for (field, value) in [("vendor", &self.vendor), ("model", &self.model)] {
            if let Some(value) = value {
                ensure!(is_name(value), "{field} 不是合法的名称");
            }
        }
        ensure!(self.ts_ms > 0, "ts_ms 必须是正的 UTC 毫秒");
        ensure!(
            self.reasoning_tokens <= self.output_tokens,
            "reasoning_tokens 是 output_tokens 的子项，不能更大"
        );
        if self.op == Op::Retract {
            ensure!(
                self.requests == 0
                    && self.input_tokens == 0
                    && self.output_tokens == 0
                    && self.cache_read_tokens == 0
                    && self.cache_write_5m_tokens == 0
                    && self.cache_write_1h_tokens == 0,
                "撤回记录的用量字段必须为 0"
            );
        }
        Ok(())
    }
}

/// 模型 / 供应商名：非空、不超长、只含 ASCII 字母数字和 `. _ - : / @ +`，
/// 不能以 `/` 开头。挡住正文、路径这类不该出现在这里的内容。
pub(crate) fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_LEN
        && !s.starts_with('/')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-:/@+".contains(&c))
}

impl UsagePage {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.v == VERSION, "不支持的版本 {}", self.v);
        ensure!(is_uuid(&self.machine_id), "machine_id 不是 UUID");
        for (i, record) in self.records.iter().enumerate() {
            record
                .validate()
                .map_err(|e| anyhow::anyhow!("第 {} 条：{e}", i + 1))?;
            ensure!(
                record.machine_id == self.machine_id,
                "第 {} 条的 machine_id 与页不一致",
                i + 1
            );
        }
        Ok(())
    }

    /// 校验并序列化；超过 `MAX_PAGE_BYTES` 报错（调用方少放几条重切）。
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_PAGE_BYTES {
            bail!("页大小 {} 超过上限 {MAX_PAGE_BYTES}", bytes.len());
        }
        Ok(bytes)
    }

    /// 导入端入口：先看大小再解析，未知字段、超限、校验不过整页拒收。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PAGE_BYTES {
            bail!("页大小 {} 超过上限 {MAX_PAGE_BYTES}", bytes.len());
        }
        let page: UsagePage = serde_json::from_slice(bytes)?;
        page.validate()?;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(host: &str) -> (tempfile::TempDir, DeviceIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), host).unwrap();
        (dir, id)
    }

    fn record(id: &DeviceIdentity, request: &str) -> UsageRecord {
        UsageRecord {
            v: VERSION,
            event_key: event_key(id, &ProviderId::ClaudeCode, EventBasis::Request(request)),
            revision: 1,
            op: Op::Upsert,
            machine_id: id.machine_id().to_string(),
            source: "claude-code".into(),
            vendor: Some("anthropic".into()),
            model: Some("claude-opus-4-5-20251101".into()),
            ts_ms: 1_763_500_000_000,
            requests: 1,
            input_tokens: 12,
            output_tokens: 340,
            cache_read_tokens: 50_000,
            cache_write_5m_tokens: 1_200,
            cache_write_1h_tokens: 800,
            reasoning_tokens: 100,
        }
    }

    type Mutate = fn(&mut UsageRecord);

    fn page(id: &DeviceIdentity, records: Vec<UsageRecord>) -> UsagePage {
        UsagePage {
            v: VERSION,
            machine_id: id.machine_id().to_string(),
            records,
        }
    }

    /// RFC 4231 测试用例 1：key = 0x0b × 20。补零到 32 字节不改变结果（短于块长的密钥本来就补零）。
    #[test]
    fn hmac_matches_reference() {
        let mut key = [0u8; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// 改主机名后重开身份，同一请求的事件键不变：重传不会被当成新请求重复计费。
    #[test]
    fn event_key_survives_hostname_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.json");
        let before = DeviceIdentity::load_or_create(&path, "old-host").unwrap();
        let after = DeviceIdentity::load_or_create(&path, "new-host").unwrap();
        let basis = EventBasis::Request("msg_01ABC");
        assert_eq!(
            event_key(&before, &ProviderId::ClaudeCode, basis),
            event_key(&after, &ProviderId::ClaudeCode, basis)
        );
    }

    #[test]
    fn event_key_distinguishes_inputs() {
        let (_d, id) = identity("h");
        let claude = ProviderId::ClaudeCode;
        let k = |source: &ProviderId, basis| event_key(&id, source, basis);
        let base = k(&claude, EventBasis::Request("msg_1"));
        assert_eq!(base.len(), 64);
        assert!(!base.contains("msg_1"), "键不透明");
        // 同一 id（Claude fork 复制的响应）同键；不同 id、不同来源、序号依据都不同键。
        assert_eq!(base, k(&claude, EventBasis::Request("msg_1")));
        assert_ne!(base, k(&claude, EventBasis::Request("msg_2")));
        assert_ne!(base, k(&ProviderId::Codex, EventBasis::Request("msg_1")));
        let ord = |s, n| {
            k(
                &claude,
                EventBasis::Ordinal {
                    native_session_id: s,
                    ordinal: n,
                },
            )
        };
        assert_ne!(ord("s", 1), ord("s", 2));
        assert_ne!(ord("ab", 1), ord("a", 1));
        // 同主机名的另一台机器密钥不同，键也不同（不会和本机的键互相覆盖）。
        let (_d2, other) = identity("h");
        assert_ne!(
            base,
            event_key(&other, &claude, EventBasis::Request("msg_1"))
        );
    }

    #[test]
    fn page_round_trips() {
        let (_d, id) = identity("h");
        let mut retract = record(&id, "msg_2");
        retract.op = Op::Retract;
        retract.revision = 2;
        retract.requests = 0;
        retract.input_tokens = 0;
        retract.output_tokens = 0;
        retract.cache_read_tokens = 0;
        retract.cache_write_5m_tokens = 0;
        retract.cache_write_1h_tokens = 0;
        retract.reasoning_tokens = 0;
        let mut no_vendor = record(&id, "msg_3");
        no_vendor.vendor = None;
        no_vendor.source = "opencode".into();
        let p = page(&id, vec![record(&id, "msg_1"), retract, no_vendor]);
        let bytes = p.encode().unwrap();
        assert_eq!(UsagePage::decode(&bytes).unwrap(), p);
    }

    /// 载荷里混进白名单外的字段（例如正文、路径）整页拒收，而不是忽略后入账。
    #[test]
    fn unknown_fields_are_rejected() {
        let (_d, id) = identity("h");
        let p = page(&id, vec![record(&id, "msg_1")]);
        let mut value = serde_json::to_value(&p).unwrap();
        value["records"][0]["cwd"] = "/home/someone/project".into();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(UsagePage::decode(&bytes).is_err());

        let mut value = serde_json::to_value(&p).unwrap();
        value["hostname"] = "box".into();
        assert!(UsagePage::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn oversized_pages_are_rejected() {
        let (_d, id) = identity("h");
        let records: Vec<_> = (0..1000)
            .map(|i| record(&id, &format!("msg_{i}")))
            .collect();
        let p = page(&id, records);
        assert!(p.encode().is_err(), "编码端不发超限页");
        let bytes = serde_json::to_vec(&p).unwrap();
        assert!(bytes.len() > MAX_PAGE_BYTES);
        assert!(UsagePage::decode(&bytes).is_err(), "导入端拒收超限页");
    }

    #[test]
    fn invalid_records_are_rejected() {
        let (_d, id) = identity("h");
        let (_d2, other) = identity("h");
        let cases: [(&str, Mutate); 10] = [
            ("版本", |r| r.v = 2),
            ("事件键", |r| r.event_key = "msg_1".into()),
            ("修订号", |r| r.revision = 0),
            ("来源", |r| r.source = "cursor".into()),
            ("时间", |r| r.ts_ms = 0),
            ("推理", |r| r.reasoning_tokens = r.output_tokens + 1),
            ("撤回带用量", |r| r.op = Op::Retract),
            ("模型带正文", |r| {
                r.model = Some("please summarize this".into())
            }),
            ("模型是路径", |r| {
                r.model = Some("/Users/me/secret".into())
            }),
            ("模型超长", |r| r.model = Some("m".repeat(129))),
        ];
        for (name, mutate) in cases {
            let mut r = record(&id, "msg_1");
            mutate(&mut r);
            assert!(page(&id, vec![r]).validate().is_err(), "{name} 应被拒收");
        }
        // 记录的机器与页不一致（把别的机器的数据混进来）。
        assert!(page(&id, vec![record(&other, "msg_1")]).validate().is_err());
    }
}
