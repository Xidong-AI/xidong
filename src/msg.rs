//! XIDONG 的记录类型与 serde 表示。
//!
//! 每条记录是一行 JSON，带标签枚举（`type` 字段）。所有 `id` 为 ULID 字符串，
//! `ts` 为 Unix 毫秒。阶段 1 冻结此 schema，详见 `docs/plan.md`。

mod record;

pub use record::{
    Ai, ApprovalDecision, OutputFiles, Record, System, ToolCall, ToolOrigin, ToolResult, User,
    is_injectable_system,
};

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
