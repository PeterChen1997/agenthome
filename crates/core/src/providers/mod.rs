//! 各 agent 的会话发现与解析。P0 只有 Claude Code；
//! 第二个 provider 落地时再抽通用 trait（见方案 §4.1），现在不预先抽象。

pub mod claude_code;
