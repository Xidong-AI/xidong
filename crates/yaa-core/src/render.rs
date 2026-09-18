//! 记录到聊天消息的渲染。
//!
//! 纯函数，无 I/O。渲染规则见 `docs/plan.md` 的「渲染管线」：
//!
//! - `tool_call(origin=hook)` + 配对 `tool_result` → `system` 消息（内容取 stdout）；
//!   `redacted` 的钩子记录（`llm`）永不渲染。
//! - `user` → `user` 消息。
//! - `ai` + 其后紧邻的 `tool_call(origin=ai)` → 一条 `assistant` 消息；每个工具调用
//!   按 `call_id` 配对 `tool_result` 转成 `tool` 消息，顺序跟随 `tool_calls`。
//! - 失配（中断）的 `ai` 工具调用合成一条「中断」`tool` 消息，保证请求体合法。
//! - `tool_call(origin=user)` → `user` 消息，绝不渲染为 `tool`。
//! - `system` → `system` 消息。
//! - 未被任何 `ai` 步骤消费的 `tool_result` 丢弃。

use std::collections::HashMap;

use yaa_msg::{Record, ToolOrigin};

use crate::message::{AssistantToolCall, ChatMessage};

/// 失配工具调用的中断标记。
const INTERRUPTED_MARKER: &str = "[interrupted] 工具结果缺失：该步骤在运行中被中断，无可用输出。";

/// 把历史记录渲染为 OpenAI 兼容的聊天消息序列。
pub fn render(records: &[Record]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut index = 0;
    while index < records.len() {
        match &records[index] {
            Record::ToolCall(call) => match call.origin {
                ToolOrigin::Hook => {
                    if !call.redacted
                        && let Some(result) = find_result(records, &call.call_id)
                    {
                        messages.push(ChatMessage::system(result.stdout.clone()));
                    }
                    index += 1;
                }
                ToolOrigin::User => {
                    messages.push(ChatMessage::user(call.command.clone()));
                    index += 1;
                }
                // 游离的 `ai` 工具调用（无相邻 `ai` 记录）无法构成助手步骤，丢弃。
                ToolOrigin::Ai => index += 1,
            },
            Record::User(user) => {
                messages.push(ChatMessage::user(user.content.clone()));
                index += 1;
            }
            Record::Ai(ai) => {
                index = push_ai_step(records, index, ai, &mut messages);
            }
            Record::System(system) => {
                messages.push(ChatMessage::system(system.content.clone()));
                index += 1;
            }
            // 未被任何 ai 步骤消费的工具结果：丢弃。
            Record::ToolResult(_) => index += 1,
        }
    }
    messages
}

/// 渲染一个 `ai` 步骤，返回下一条待处理记录的下标。
fn push_ai_step(
    records: &[Record],
    ai_index: usize,
    ai: &yaa_msg::Ai,
    messages: &mut Vec<ChatMessage>,
) -> usize {
    let mut calls: Vec<&yaa_msg::ToolCall> = Vec::new();
    let mut next = ai_index + 1;
    while let Some(Record::ToolCall(call)) = records.get(next) {
        if call.origin != ToolOrigin::Ai {
            break;
        }
        calls.push(call);
        next += 1;
    }

    let content = if ai.content.is_empty() {
        None
    } else {
        Some(ai.content.clone())
    };
    let tool_calls = if calls.is_empty() {
        None
    } else {
        Some(
            calls
                .iter()
                .map(|call| AssistantToolCall {
                    id: call.call_id.clone(),
                    // 阶段 1 仅暴露 `shell` 一个工具，记录未保存函数名，渲染时统一回填。
                    name: "shell".to_string(),
                    arguments: call.command.clone(),
                })
                .collect(),
        )
    };
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content,
        tool_calls,
        tool_call_id: None,
    });

    // 紧随工具调用的 `tool_result` 运行段，按 call_id 配对。
    let mut results: HashMap<&str, &yaa_msg::ToolResult> = HashMap::new();
    let mut end = next;
    while let Some(Record::ToolResult(result)) = records.get(end) {
        results.entry(result.call_id.as_str()).or_insert(result);
        end += 1;
    }

    for call in &calls {
        match results.get(call.call_id.as_str()) {
            Some(result) => messages.push(ChatMessage::tool_result(
                call.call_id.clone(),
                tool_content(result),
            )),
            None => messages.push(ChatMessage::tool_result(
                call.call_id.clone(),
                INTERRUPTED_MARKER,
            )),
        }
    }
    end
}

/// 在全部记录中查找首个匹配 `call_id` 的工具结果。
fn find_result<'a>(records: &'a [Record], call_id: &str) -> Option<&'a yaa_msg::ToolResult> {
    records.iter().find_map(|record| match record {
        Record::ToolResult(result) if result.call_id == call_id => Some(result),
        _ => None,
    })
}

/// 组装 `tool` 消息内容：stdout，追加独立标注的 stderr，再追加超时/截断提示。
fn tool_content(result: &yaa_msg::ToolResult) -> String {
    let mut content = result.stdout.clone();
    if !result.stderr.is_empty() {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("[stderr]\n");
        content.push_str(&result.stderr);
    }
    if result.timed_out {
        content.push_str("\n[timeout] 命令超时，已被终止。");
    }
    if result.truncated {
        content.push_str("\n[truncated] 仅保留头尾输出，完整内容见输出文件。");
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaa_msg::{HookInfo, ToolCall, ToolResult};

    fn hook_call(call_id: &str, name: &str, redacted: bool) -> Record {
        Record::ToolCall(ToolCall {
            id: format!("id-{call_id}"),
            ts: 1,
            origin: ToolOrigin::Hook,
            call_id: call_id.to_string(),
            command: "echo prompt".to_string(),
            description: None,
            hook: Some(HookInfo {
                name: name.to_string(),
                fingerprint: "fp".to_string(),
            }),
            redacted,
        })
    }

    fn result(call_id: &str, stdout: &str) -> Record {
        Record::ToolResult(ToolResult {
            id: format!("res-{call_id}"),
            ts: 2,
            call_id: call_id.to_string(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
            truncated: false,
            output_files: None,
        })
    }

    fn ai_call(call_id: &str, arguments: &str) -> Record {
        Record::ToolCall(ToolCall {
            id: format!("ai-{call_id}"),
            ts: 3,
            origin: ToolOrigin::Ai,
            call_id: call_id.to_string(),
            command: arguments.to_string(),
            description: None,
            hook: None,
            redacted: false,
        })
    }

    fn roles(messages: &[ChatMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.role.as_str()).collect()
    }

    #[test]
    fn hook_pair_then_user_then_ai() {
        let records = vec![
            hook_call("h1", "system_prompt", false),
            result("h1", "SYS"),
            Record::user("你好"),
            Record::ai("m", "h", "回复"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system", "user", "assistant"]);
        assert_eq!(messages[0].content.as_deref(), Some("SYS"));
        assert_eq!(messages[1].content.as_deref(), Some("你好"));
        assert_eq!(messages[2].content.as_deref(), Some("回复"));
    }

    #[test]
    fn ai_with_two_calls_and_two_results_orders_by_calls() {
        let records = vec![
            Record::ai("m", "h", ""),
            ai_call("c1", r#"{"command":"a"}"#),
            ai_call("c2", r#"{"command":"b"}"#),
            // 结果顺序故意颠倒，渲染须按 tool_calls 顺序排列。
            result("c2", "B"),
            result("c1", "A"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["assistant", "tool", "tool"]);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[1].content.as_deref(), Some("A"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(messages[2].content.as_deref(), Some("B"));
        assert_eq!(messages[0].tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn missing_result_synthesizes_interrupted_tool() {
        let records = vec![
            Record::ai("m", "h", ""),
            ai_call("orphan", r#"{"command":"sleep"}"#),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["assistant", "tool"]);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("orphan"));
        assert!(
            messages[1]
                .content
                .as_deref()
                .unwrap()
                .contains("interrupted")
        );
    }

    #[test]
    fn redacted_hook_is_skipped() {
        let records = vec![
            hook_call("llm", "llm", true),
            Record::system("src", "scope", "正文"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system"]);
        assert_eq!(messages[0].content.as_deref(), Some("正文"));
    }

    #[test]
    fn user_origin_call_renders_as_user() {
        let records = vec![Record::ToolCall(ToolCall {
            id: "u".to_string(),
            ts: 1,
            origin: ToolOrigin::User,
            call_id: "u1".to_string(),
            command: "ls".to_string(),
            description: None,
            hook: None,
            redacted: false,
        })];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user"]);
        assert_eq!(messages[0].content.as_deref(), Some("ls"));
    }

    #[test]
    fn plain_system_record_renders() {
        let messages = render(&[Record::system("s", "scope", "内容")]);
        assert_eq!(roles(&messages), ["system"]);
        assert_eq!(messages[0].content.as_deref(), Some("内容"));
    }

    #[test]
    fn unconsumed_tool_result_is_dropped() {
        let messages = render(&[result("nobody", "stray")]);
        assert!(messages.is_empty());
    }

    #[test]
    fn stderr_and_timeout_and_truncation_are_labelled() {
        let records = vec![
            Record::ai("m", "h", ""),
            ai_call("c1", r#"{"command":"x"}"#),
            Record::ToolResult(ToolResult {
                id: "r".to_string(),
                ts: 2,
                call_id: "c1".to_string(),
                stdout: "out".to_string(),
                stderr: "err".to_string(),
                code: 1,
                timed_out: true,
                truncated: true,
                output_files: None,
            }),
        ];
        let messages = render(&records);
        let content = messages[1].content.as_deref().unwrap();
        assert!(content.contains("out"));
        assert!(content.contains("[stderr]"));
        assert!(content.contains("err"));
        assert!(content.contains("[timeout]"));
        assert!(content.contains("[truncated]"));
    }
}
