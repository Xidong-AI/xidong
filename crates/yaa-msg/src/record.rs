//! 记录类型定义（阶段 1 冻结 schema）。
//!
//! 内部标签枚举，`type` 取值 `user` / `ai` / `tool_call` / `tool_result` / `system`。
//! 所有记录都带唯一 `id`（阶段 4 幂等去重），`tool_result` 另用 `call_id` 配对
//! `tool_call`。`approval_pending` / `approval_resolved` 属阶段 2，此处刻意不定义，
//! 以免引入当前无消费者的死分支。

use serde::{Deserialize, Serialize};

use crate::now_ms;

/// 一条历史记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    User(User),
    Ai(Ai),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    System(System),
}

impl Record {
    /// 全局唯一的记录 id。
    pub fn id(&self) -> &str {
        match self {
            Record::User(r) => &r.id,
            Record::Ai(r) => &r.id,
            Record::ToolCall(r) => &r.id,
            Record::ToolResult(r) => &r.id,
            Record::System(r) => &r.id,
        }
    }

    /// 用户消息。
    pub fn user(content: impl Into<String>) -> Self {
        Record::User(User {
            id: new_id(),
            ts: now_ms(),
            content: content.into(),
            client_id: None,
            turn_id: None,
        })
    }

    /// 模型回复。`reasoning` 为可选思考内容，落盘、可展示，是否回传由端点格式决定。
    pub fn ai(
        model: impl Into<String>,
        tool_schema_hash: impl Into<String>,
        content: impl Into<String>,
        reasoning: Option<String>,
    ) -> Self {
        Record::Ai(Ai {
            id: new_id(),
            ts: now_ms(),
            model: model.into(),
            tool_schema_hash: tool_schema_hash.into(),
            content: content.into(),
            reasoning,
        })
    }

    /// 工具调用（`origin` 标明来源）。
    pub fn tool_call(
        origin: ToolOrigin,
        call_id: impl Into<String>,
        command: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        Record::ToolCall(ToolCall {
            id: new_id(),
            ts: now_ms(),
            origin,
            call_id: call_id.into(),
            command: command.into(),
            description,
        })
    }

    /// 工具结果。
    // 字段一一映射冻结的记录 schema，拆成结构体反而让调用点更啰嗦，故显式豁免。
    #[allow(clippy::too_many_arguments)]
    pub fn tool_result(
        call_id: impl Into<String>,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        code: i32,
        timed_out: bool,
        truncated: bool,
        output_files: Option<OutputFiles>,
    ) -> Self {
        Record::ToolResult(ToolResult {
            id: new_id(),
            ts: now_ms(),
            call_id: call_id.into(),
            stdout: stdout.into(),
            stderr: stderr.into(),
            code,
            timed_out,
            truncated,
            output_files,
        })
    }

    /// 系统消息（通知、初始化簿记等）。`command` 可选，只用于展示，不进入上下文。
    pub fn system(
        source: impl Into<String>,
        content: impl Into<String>,
        command: Option<String>,
    ) -> Self {
        Record::System(System {
            id: new_id(),
            ts: now_ms(),
            source: source.into(),
            command,
            content: content.into(),
        })
    }
}

fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// 用户记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub ts: i64,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

/// 模型回复记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ai {
    pub id: String,
    pub ts: i64,
    pub model: String,
    pub tool_schema_hash: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// 工具调用来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolOrigin {
    Ai,
    User,
}

/// 工具调用记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub ts: i64,
    pub origin: ToolOrigin,
    pub call_id: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 工具结果记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub ts: i64,
    pub call_id: String,
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_files: Option<OutputFiles>,
}

/// 截断后完整输出的外部文件，按流分开。
///
/// 路径相对**数据根**，因此会话归档后仍可解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFiles {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

/// 系统记录。`command` 可选，只用于展示，不进入上下文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct System {
    pub id: String,
    pub ts: i64,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(r: &Record) -> Record {
        let line = serde_json::to_string(r).expect("serialize");
        serde_json::from_str(&line).expect("deserialize")
    }

    #[test]
    fn user_golden_and_round_trip() {
        let r = Record::User(User {
            id: "01JUSER".into(),
            ts: 1000,
            content: "你好".into(),
            client_id: None,
            turn_id: None,
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"user","id":"01JUSER","ts":1000,"content":"你好"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn user_optional_fields_serialize_when_present() {
        let r = Record::User(User {
            id: "01JUSER".into(),
            ts: 1,
            content: "hi".into(),
            client_id: Some("cli_x".into()),
            turn_id: Some("turn_x".into()),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"user","id":"01JUSER","ts":1,"content":"hi","client_id":"cli_x","turn_id":"turn_x"}"#
        );
    }

    #[test]
    fn ai_golden_without_reasoning() {
        let r = Record::Ai(Ai {
            id: "01JAI".into(),
            ts: 2,
            model: "deepseek-chat".into(),
            tool_schema_hash: "abc".into(),
            content: "在".into(),
            reasoning: None,
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"ai","id":"01JAI","ts":2,"model":"deepseek-chat","tool_schema_hash":"abc","content":"在"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn ai_golden_with_reasoning() {
        let r = Record::Ai(Ai {
            id: "01JAI".into(),
            ts: 2,
            model: "deepseek-chat".into(),
            tool_schema_hash: "abc".into(),
            content: "在".into(),
            reasoning: Some("想".into()),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"ai","id":"01JAI","ts":2,"model":"deepseek-chat","tool_schema_hash":"abc","content":"在","reasoning":"想"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn tool_call_golden_and_round_trip() {
        let r = Record::ToolCall(ToolCall {
            id: "01JCALL".into(),
            ts: 3,
            origin: ToolOrigin::Ai,
            call_id: "call_1".into(),
            command: "echo hi".into(),
            description: None,
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"tool_call","id":"01JCALL","ts":3,"origin":"ai","call_id":"call_1","command":"echo hi"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn tool_call_golden_with_description() {
        let r = Record::ToolCall(ToolCall {
            id: "01JCALL".into(),
            ts: 3,
            origin: ToolOrigin::Ai,
            call_id: "call_1".into(),
            command: "echo hi".into(),
            description: Some("打招呼".into()),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"tool_call","id":"01JCALL","ts":3,"origin":"ai","call_id":"call_1","command":"echo hi","description":"打招呼"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn tool_origin_is_lowercase() {
        for (origin, tag) in [(ToolOrigin::Ai, "ai"), (ToolOrigin::User, "user")] {
            assert_eq!(
                serde_json::to_string(&origin).expect("serialize"),
                format!("\"{tag}\"")
            );
        }
    }

    #[test]
    fn tool_result_golden_and_round_trip() {
        let r = Record::ToolResult(ToolResult {
            id: "01JRES".into(),
            ts: 4,
            call_id: "call_1".into(),
            stdout: "out".into(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
            truncated: true,
            output_files: Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/01JFILE.stdout.out".into()),
                stderr: None,
            }),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"tool_result","id":"01JRES","ts":4,"call_id":"call_1","stdout":"out","stderr":"","code":0,"timed_out":false,"truncated":true,"output_files":{"stdout":"sessions/sid/outputs/01JFILE.stdout.out"}}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn tool_result_omits_absent_output_files() {
        let r = Record::ToolResult(ToolResult {
            id: "01JRES".into(),
            ts: 4,
            call_id: "call_1".into(),
            stdout: "out".into(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
            truncated: false,
            output_files: None,
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert!(!line.contains("output_files"));
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn tool_result_keeps_both_stream_files() {
        let r = Record::ToolResult(ToolResult {
            id: "01JRES".into(),
            ts: 4,
            call_id: "call_1".into(),
            stdout: "o".into(),
            stderr: "e".into(),
            code: 1,
            timed_out: false,
            truncated: true,
            output_files: Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/a.stdout.out".into()),
                stderr: Some("sessions/sid/outputs/a.stderr.out".into()),
            }),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert!(line.contains(r#""stdout":"sessions/sid/outputs/a.stdout.out""#));
        assert!(line.contains(r#""stderr":"sessions/sid/outputs/a.stderr.out""#));
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn system_golden_with_command() {
        let r = Record::System(System {
            id: "01JSYS".into(),
            ts: 5,
            source: "notify".into(),
            command: Some("git status".into()),
            content: "……".into(),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"system","id":"01JSYS","ts":5,"source":"notify","command":"git status","content":"……"}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn system_golden_without_command() {
        let r = Record::System(System {
            id: "01JSYS".into(),
            ts: 5,
            source: "init".into(),
            command: None,
            content: String::new(),
        });
        let line = serde_json::to_string(&r).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"system","id":"01JSYS","ts":5,"source":"init","content":""}"#
        );
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn constructors_assign_unique_ids() {
        let a = Record::user("a");
        let b = Record::user("b");
        assert_ne!(a.id(), b.id());
        assert!(!a.id().is_empty());
    }
}
