//! 内置来源注册表：AgentHome 支持哪些 agent，全部在这一张表里。
//!
//! 同步引擎、命令行、仪表盘都遍历它，不再各自写死来源列表。加一个新来源（如 OpenCode）：
//! 1. 在 `providers/` 下新建一个文件，实现 [`SourceProvider`]（「发现文件」+「解析一个文件」）；
//! 2. 在 `model.rs` 的 `ProviderId` 里加一个 id（存库用的字符串一旦发布就不要改）；
//! 3. 在下面的 [`builtin`] 里登记一行；
//! 4. 照 `antigravity_cli.rs` 末尾的测试写一份合成 fixture 测试。
//!
//! 详见 `docs/13-来源插件开发指南.md`。

use crate::model::ProviderId;
use crate::paths;
use crate::providers::SourceProvider;
use crate::providers::antigravity_cli::AntigravityCliProvider;
use crate::providers::claude_code::ClaudeCodeProvider;
use crate::providers::codex::CodexProvider;
use crate::providers::gemini::GeminiProvider;
use std::path::PathBuf;

/// 栖息地里代表这个来源的住户。没有对应住户的来源（`None`）照常入库、出现在归档与统计里，
/// 只是栖息地不画它。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Resident {
    Claude,
    Codex,
    Gemini,
}

/// 一个内置来源的描述。
pub struct ProviderSpec {
    pub id: ProviderId,
    /// 界面与命令行里显示的名字。
    pub name: &'static str,
    pub resident: Option<Resident>,
    /// 恢复会话的命令模板，`{id}` 换成会话的原生 id；不支持恢复时为 None。
    pub resume_command: Option<&'static str>,
    /// 本机默认的数据目录。
    pub default_root: fn() -> PathBuf,
    /// 用给定根目录构造解析器（命令行可覆盖根目录）。
    pub build: fn(PathBuf) -> Box<dyn SourceProvider + Send + Sync>,
}

impl ProviderSpec {
    /// 用默认根目录构造。
    pub fn instantiate(&self) -> Box<dyn SourceProvider + Send + Sync> {
        (self.build)((self.default_root)())
    }
}

/// 全部内置来源，顺序即界面里的展示顺序。
pub fn builtin() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            id: ProviderId::ClaudeCode,
            name: "Claude Code",
            resident: Some(Resident::Claude),
            resume_command: Some("claude --resume {id}"),
            default_root: paths::claude_projects_dir,
            build: |root| Box::new(ClaudeCodeProvider::new(root)),
        },
        ProviderSpec {
            id: ProviderId::Codex,
            name: "Codex",
            resident: Some(Resident::Codex),
            resume_command: Some("codex resume {id}"),
            default_root: paths::codex_sessions_dir,
            build: |root| Box::new(CodexProvider::new(root)),
        },
        ProviderSpec {
            id: ProviderId::GeminiCli,
            name: "Gemini CLI",
            resident: Some(Resident::Gemini),
            resume_command: None,
            default_root: paths::gemini_dir,
            build: |root| Box::new(GeminiProvider::new(root)),
        },
        ProviderSpec {
            id: ProviderId::AntigravityCli,
            name: "Antigravity CLI",
            resident: Some(Resident::Gemini),
            resume_command: Some("agy --conversation {id}"),
            default_root: paths::antigravity_cli_dir,
            build: |root| Box::new(AntigravityCliProvider::new(root)),
        },
    ]
}

/// 按 id 查内置来源。
pub fn spec(id: &ProviderId) -> Option<ProviderSpec> {
    builtin().into_iter().find(|s| &s.id == id)
}

/// 来源的显示名；不在注册表里的（旧库里残留的、以后的外部插件）用存库字符串本身。
pub fn display_name(id: &ProviderId) -> String {
    spec(id).map_or_else(|| id.as_str().to_string(), |s| s.name.to_string())
}

/// 用默认根目录构造全部内置来源。
pub fn instantiate_all() -> Vec<Box<dyn SourceProvider + Send + Sync>> {
    builtin().iter().map(ProviderSpec::instantiate).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 注册表里的来源各不相同_且解析器的_id_与登记一致() {
        // 登记错 id 会让会话存到别的来源名下、统计串台。
        let specs = builtin();
        for (i, a) in specs.iter().enumerate() {
            assert!(
                specs[i + 1..].iter().all(|b| b.id != a.id),
                "{:?} 重复",
                a.id
            );
            let dir = tempfile::tempdir().unwrap();
            assert_eq!((a.build)(dir.path().to_path_buf()).id(), a.id);
            assert_eq!(ProviderId::parse(a.id.as_str()), a.id, "存库字符串要能读回");
        }
    }

    #[test]
    fn 不在注册表的来源也有名字可显示() {
        assert_eq!(display_name(&ProviderId::AntigravityCli), "Antigravity CLI");
        assert_eq!(display_name(&ProviderId::Other("pi".into())), "pi");
    }
}
