//! 记录到聊天消息的渲染。
//!
//! 纯函数，无 I/O。渲染规则见 `docs/plan.md` 的「渲染管线与分层装配」：
//!
//! - 首条 `user` 之前的**所有非空 `system`**（`source=init` 与空内容除外）按物理顺序
//!   以空行拼接为**一条** `system` 消息，置于消息列表最前（无则不加）。
//! - 首条 `user` 起，`user` 与「非空 `system`」组成的**最大连续段**按物理顺序以空行
//!   拼接为**一条** `user` 消息（双向合并）；段外的通知自然单独成一条 `user` 消息。
//! - `ai` + 其后紧邻的 `tool_call(origin=ai)` → 一条 `assistant` 消息；工具调用参数由
//!   记录的 `command`/`description` 重建为规范 JSON。
//! - 配对 `tool_result` → `tool` 消息（`tool_call_id` 关联）：**全日志一次建表**
//!   （`call_id` → 首个结果），不要求结果紧邻 `tool_call`；顺序跟随 `tool_calls`。
//! - 失配的 `ai` 工具调用合成一条 `tool` 消息，保证请求体合法：若该 `call_id` 有
//!   `approval_pending` 且无 `approval_resolved` 且无 `tool_result`，合成「待批准、尚未
//!   执行」占位；否则合成中断标记。
//! - `tool_call(origin=user)` → `user` 消息，**绝不**渲染为 `tool`。
//! - `system(source=init)`、空 `system`、空 `ai`、游离调用与未被消费的结果不投影。

use std::collections::{HashMap, HashSet};

use yaa_msg::{Record, ToolOrigin};

use crate::display;
use crate::message::{AssistantToolCall, ChatMessage};
use crate::tools::shell;

/// 失配工具调用的中断标记。
const INTERRUPTED_MARKER: &str = "[interrupted] 工具结果缺失：该步骤在运行中被中断，无可用输出。";

/// 未决待批工具调用的占位标记：等待批准，尚未执行。
const PENDING_MARKER: &str = "[pending] 该工具调用等待批准，尚未执行。";

/// 把历史记录渲染为 OpenAI 兼容的聊天消息序列。
pub fn render(records: &[Record]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();

    // 全日志一次建表：`call_id` → 首个 `tool_result`。配对不要求紧邻，因此结果与
    // `tool_call` 之间夹着 `system(notify)`（外部通知）或后续记录时仍能正确配对。
    let mut results: HashMap<&str, &yaa_msg::ToolResult> = HashMap::new();
    // 审批配对：有 `approval_pending` 且无 `approval_resolved` 且无 `tool_result` 的
    // `call_id` 为未决待批，渲染为待批占位而非中断（D43）。
    let mut pending_ids: HashSet<&str> = HashSet::new();
    let mut resolved_ids: HashSet<&str> = HashSet::new();
    for record in records {
        match record {
            Record::ToolResult(result) => {
                results.entry(result.call_id.as_str()).or_insert(result);
            }
            Record::ApprovalPending(approval) => {
                pending_ids.insert(approval.call_id.as_str());
            }
            Record::ApprovalResolved(approval) => {
                resolved_ids.insert(approval.call_id.as_str());
            }
            _ => {}
        }
    }

    // 稳定前缀：首条 `user` 之前的非空 `system`（`init` 簿记与空内容除外）拼成一条。
    //
    // 注意：本前缀只收集 `System`，首条 `user` 之前若出现 `tool_call(origin=user)`，
    // 主循环从 `first_user` 起会将其丢弃。阶段 1/2 无该记录的生产者；若将来引入，
    // 需在此一并处理。
    let first_user = records
        .iter()
        .position(|record| matches!(record, Record::User(_)));
    let prefix_end = first_user.unwrap_or(records.len());
    let prefix: Vec<String> = records[..prefix_end]
        .iter()
        .filter_map(|record| match record {
            Record::System(system) if is_projectable_system(system) => Some(project_system(system)),
            _ => None,
        })
        .collect();
    if !prefix.is_empty() {
        messages.push(ChatMessage::system(prefix.join("\n\n")));
    }

    // 无 `user` 记录时，前导 `system` 已全部并入稳定前缀，主循环从头部继续处理
    // 其余记录（`tool_call(origin=user)`、`ai` 等），但不得重复投影这些 `system`。
    let skip_prefix_systems = first_user.is_none();
    let mut index = first_user.unwrap_or(0);

    while index < records.len() {
        if skip_prefix_systems && matches!(records[index], Record::System(_)) {
            index += 1;
            continue;
        }
        if is_user_run_item(&records[index]) {
            // 双向最大连续段：`user` 与非空 `system` 交替出现时并入同一条消息。
            let mut parts: Vec<String> = Vec::new();
            while index < records.len() {
                match &records[index] {
                    Record::User(user) => {
                        parts.push(user.content.clone());
                        index += 1;
                    }
                    Record::System(system) if is_projectable_system(system) => {
                        parts.push(project_system(system));
                        index += 1;
                    }
                    _ => break,
                }
            }
            messages.push(ChatMessage::user(parts.join("\n\n")));
            continue;
        }

        match &records[index] {
            Record::Ai(ai) => {
                index = push_ai_step(
                    records,
                    index,
                    ai,
                    &results,
                    &pending_ids,
                    &resolved_ids,
                    &mut messages,
                );
            }
            Record::ToolCall(call) if call.origin == ToolOrigin::User => {
                messages.push(ChatMessage::user(call.command.clone()));
                index += 1;
            }
            // `system(source=init)`、空 `system`、游离调用与孤儿结果：不投影。
            // 已配对的 `tool_result` 已在对应助手步骤内发出，这里一律跳过。
            // 审批记录（阶段 2）不参与模型消息投影，也不改变 `assistant`/`tool` 配对。
            Record::ApprovalPending(_) | Record::ApprovalResolved(_) => index += 1,
            _ => index += 1,
        }
    }

    messages
}

/// 该记录是否参与「`user` + 非空 `system`」连续段。
fn is_user_run_item(record: &Record) -> bool {
    match record {
        Record::User(_) => true,
        Record::System(system) => is_projectable_system(system),
        _ => false,
    }
}

/// 非空的普通 `system`；`init` 是初始化簿记。
///
/// 非空判定同时看 `content` 与 `output_files`：正文被截断、`content` 为空但有完整
/// 输出文件时同样要投影。
fn is_projectable_system(system: &yaa_msg::System) -> bool {
    (!system.content.is_empty() || system.output_files.is_some()) && system.source != "init"
}

/// 把一个 `system` 投影为进入上下文的文本。
///
/// 正文取 [`display::system_text`]（含截断提示）；若记录带 `command`，在最前拼
/// `[执行命令] <command>`，让模型知道通知由哪条命令产生（T1）。
fn project_system(system: &yaa_msg::System) -> String {
    let mut text = String::new();
    if let Some(command) = &system.command {
        text.push_str(&format!("[执行命令] {command}\n"));
    }
    text.push_str(&display::system_text(system));
    text
}

/// 渲染一个 `ai` 步骤，返回下一条待处理记录的下标。
///
/// `results` 是全日志建好的 `call_id` → `tool_result` 表，因此配对不要求紧邻：
/// 结果可以夹着 `system(notify)` 或出现在更后面。结果消息的发出顺序严格跟随
/// `tool_calls` 顺序。
#[allow(clippy::too_many_arguments)] // 传引用表以避免在循环内重复建表
fn push_ai_step(
    records: &[Record],
    ai_index: usize,
    ai: &yaa_msg::Ai,
    results: &HashMap<&str, &yaa_msg::ToolResult>,
    pending_ids: &HashSet<&str>,
    resolved_ids: &HashSet<&str>,
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
        Some(calls.iter().map(|call| rebuild_tool_call(call)).collect())
    };
    if content.is_none() && tool_calls.is_none() {
        // 空 `ai` 记录（无文本、无工具调用）不构成助手消息；跳过以免请求体非法。
        return next;
    }
    messages.push(ChatMessage::assistant(
        content,
        tool_calls,
        ai.reasoning.clone(),
    ));

    for call in &calls {
        let call_id = call.call_id.as_str();
        let content = match results.get(call_id) {
            Some(result) => tool_content(result),
            // 未决待批（有 pending、无 resolved、无 result）表达「等待批准，尚未执行」。
            None if pending_ids.contains(call_id) && !resolved_ids.contains(call_id) => {
                PENDING_MARKER.to_string()
            }
            None => INTERRUPTED_MARKER.to_string(),
        };
        messages.push(ChatMessage::tool_result(call.call_id.clone(), content));
    }
    next
}

/// 由记录的 `command`/`description` 重建规范 JSON 参数。
///
/// 记录不保存模型的原始 `arguments` 文本，渲染时以这两个字段重建；
/// `serde_json::Map` 默认按字典序输出，故 `description` 存在时键序稳定。
fn rebuild_tool_call(call: &yaa_msg::ToolCall) -> AssistantToolCall {
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        "command".to_string(),
        serde_json::Value::String(call.command.clone()),
    );
    if let Some(description) = &call.description {
        arguments.insert(
            "description".to_string(),
            serde_json::Value::String(description.clone()),
        );
    }
    AssistantToolCall {
        id: call.call_id.clone(),
        name: shell::NAME.to_string(),
        arguments: serde_json::Value::Object(arguments).to_string(),
    }
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
        content.push('\n');
        let empty = yaa_msg::OutputFiles {
            stdout: None,
            stderr: None,
        };
        content.push_str(&display::truncation_note(
            result.output_files.as_ref().unwrap_or(&empty),
        ));
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaa_msg::{OutputFiles, ToolResult};

    fn notify(content: &str) -> Record {
        Record::system("notify", content, None, None)
    }

    fn notify_with_command(content: &str, command: &str) -> Record {
        Record::system("notify", content, Some(command.to_string()), None)
    }

    fn init() -> Record {
        Record::system("init", "", None, None)
    }

    fn result(call_id: &str, stdout: &str) -> Record {
        Record::tool_result(call_id, stdout, "", 0, false, false, None)
    }

    fn roles(messages: &[ChatMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.role.as_str()).collect()
    }

    #[test]
    fn stable_prefix_merges_systems_before_first_user() {
        let records = vec![notify("系统提示词"), notify("环境"), Record::user("你好")];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system", "user"]);
        assert_eq!(messages[0].content.as_deref(), Some("系统提示词\n\n环境"));
        assert_eq!(messages[1].content.as_deref(), Some("你好"));
    }

    #[test]
    fn empty_and_init_systems_do_not_project() {
        let records = vec![notify(""), init(), Record::user("你好")];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user"]);
        assert_eq!(messages[0].content.as_deref(), Some("你好"));
    }

    #[test]
    fn bidirectional_maximal_run_merges_on_both_sides() {
        // 首条 user 两侧的通知都并入同一条 user 消息。
        let records = vec![notify("前缀"), Record::user("问题"), notify("补充")];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system", "user"]);
        assert_eq!(messages[0].content.as_deref(), Some("前缀"));
        assert_eq!(messages[1].content.as_deref(), Some("问题\n\n补充"));
    }

    #[test]
    fn trailing_notification_after_ai_is_separate_user() {
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "回复", None),
            notify("后台结果"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "user"]);
        assert_eq!(messages[0].content.as_deref(), Some("你好"));
        assert_eq!(messages[2].content.as_deref(), Some("后台结果"));
    }

    #[test]
    fn envelope_is_preserved_verbatim_in_merged_user() {
        let records = vec![
            Record::user("问题"),
            notify_with_command("<环境>\n工作目录：/tmp\n</环境>", "pwd"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user"]);
        assert_eq!(
            messages[0].content.as_deref(),
            Some("问题\n\n[执行命令] pwd\n<环境>\n工作目录：/tmp\n</环境>")
        );
    }

    #[test]
    fn command_prefix_is_prepended_in_stable_prefix_and_user_run() {
        let records = vec![
            notify_with_command("<系统提示词>\n正文\n</系统提示词>", "git status"),
            Record::user("问题"),
            notify_with_command("<补充>\n更多\n</补充>", "echo hi"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system", "user"]);
        assert_eq!(
            messages[0].content.as_deref(),
            Some("[执行命令] git status\n<系统提示词>\n正文\n</系统提示词>")
        );
        assert_eq!(
            messages[1].content.as_deref(),
            Some("问题\n\n[执行命令] echo hi\n<补充>\n更多\n</补充>")
        );
    }

    #[test]
    fn projection_body_equals_display_system_text_for_commandless_notify() {
        // 无 `command` 时，展示侧 `Injection.content` 与渲染正文必须逐字一致（D29）。
        let record = Record::system(
            "notify",
            "<通知>\n正文\n</通知>",
            None,
            Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/01J.stdout.out".into()),
                stderr: None,
            }),
        );
        let Record::System(system) = &record else {
            panic!("应为 system");
        };
        let projected = &display::system_text(system);

        let messages = render(std::slice::from_ref(&record));
        assert_eq!(messages[0].content.as_deref(), Some(projected.as_str()));

        let items = display::items(std::slice::from_ref(&record));
        let display::DisplayItem::Injection { content, .. } = &items[0] else {
            panic!("应投影为 Injection");
        };
        assert_eq!(content, projected, "展示正文应与渲染正文一致");
    }

    #[test]
    fn truncation_note_with_relative_path_is_projected() {
        let records = vec![Record::system(
            "notify",
            "<通知>\n头尾\n</通知>",
            Some("seq 1 9".into()),
            Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/01J.stdout.out".into()),
                stderr: None,
            }),
        )];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["system"]);
        let content = messages[0].content.as_deref().unwrap();
        assert!(content.starts_with("[执行命令] seq 1 9\n"));
        assert!(
            content.contains(
                "[truncated] 仅保留头尾输出，完整内容见输出文件 sessions/sid/outputs/01J.stdout.out。"
            ),
            "截断提示应含相对路径：{content}"
        );
    }

    #[test]
    fn ai_step_rebuilds_arguments_and_pairs_results() {
        let records = vec![
            Record::ai("m", "h", "", Some("思考".into())),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo a", Some("打招呼".into())),
            Record::tool_call(ToolOrigin::Ai, "c2", "echo b", None),
            // 结果顺序故意颠倒，渲染须按 tool_calls 顺序排列。
            result("c2", "B"),
            result("c1", "A"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["assistant", "tool", "tool"]);
        assert_eq!(messages[0].reasoning.as_deref(), Some("思考"));

        let calls = messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments,
            r#"{"command":"echo a","description":"打招呼"}"#
        );
        assert_eq!(calls[1].arguments, r#"{"command":"echo b"}"#);

        assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[1].content.as_deref(), Some("A"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(messages[2].content.as_deref(), Some("B"));
    }

    #[test]
    fn missing_result_synthesizes_interrupted_tool() {
        let records = vec![
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "orphan", "sleep", None),
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
    fn ai_reasoning_enters_assistant_message() {
        let records = vec![Record::ai("m", "h", "正文", Some("推理".into()))];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["assistant"]);
        assert_eq!(messages[0].content.as_deref(), Some("正文"));
        assert_eq!(messages[0].reasoning.as_deref(), Some("推理"));
    }

    #[test]
    fn empty_ai_record_renders_no_message() {
        let messages = render(&[Record::ai("m", "h", "", None)]);
        assert!(messages.is_empty());
    }

    #[test]
    fn user_origin_call_renders_as_user_never_tool() {
        let records = vec![Record::tool_call(ToolOrigin::User, "u1", "ls", None)];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user"]);
        assert_eq!(messages[0].content.as_deref(), Some("ls"));
    }

    #[test]
    fn stray_ai_call_and_orphan_result_are_dropped() {
        let records = vec![
            Record::tool_call(ToolOrigin::Ai, "stray", "ls", None),
            result("nobody", "stray"),
        ];
        let messages = render(&records);
        assert!(messages.is_empty());
    }

    #[test]
    fn result_separated_by_notify_still_pairs_and_notify_projects_user() {
        // `[user, ai, tool_call, system(notify), tool_result]`：结果不紧邻 `tool_call`，
        // 仍须按 call_id 全日志配对；notify 由主循环按连续段正常投影为 user。
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo", None),
            notify("后台结果"),
            result("c1", "REAL"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "tool", "user"]);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(
            messages[2].content.as_deref(),
            Some("REAL"),
            "夹着 notify 的结果必须配对为真实输出，而非中断标记"
        );
        assert_eq!(messages[3].content.as_deref(), Some("后台结果"));
    }

    #[test]
    fn result_appearing_later_in_log_still_pairs() {
        // 结果出现在更后面（中间夹了一条 user）：全日志建表仍能配对。
        let records = vec![
            Record::user("第一问"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo", None),
            Record::user("继续"),
            result("c1", "LATE"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "tool", "user"]);
        assert_eq!(messages[2].content.as_deref(), Some("LATE"));
        assert_eq!(messages[3].content.as_deref(), Some("继续"));
    }

    #[test]
    fn stray_calls_are_not_attached_to_previous_step() {
        // 结果之后再出现的 tool_call 不构成新步骤，作为游离记录丢弃。
        let records = vec![
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "a", None),
            result("c1", "A"),
            Record::tool_call(ToolOrigin::Ai, "c2", "b", None),
            result("c2", "B"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["assistant", "tool"]);
        let calls = messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1, "只有紧邻 ai 的 c1 进入 tool_calls");
        assert_eq!(calls[0].id, "c1");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn stderr_timeout_and_truncation_are_labelled() {
        let records = vec![
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "x", None),
            Record::ToolResult(ToolResult {
                id: "r".to_string(),
                ts: 2,
                call_id: "c1".to_string(),
                stdout: "out".to_string(),
                stderr: "err".to_string(),
                code: 1,
                timed_out: true,
                truncated: true,
                output_files: Some(OutputFiles {
                    stdout: Some("sessions/sid/out.stdout.out".to_string()),
                    stderr: None,
                }),
            }),
        ];
        let messages = render(&records);
        let content = messages[1].content.as_deref().unwrap();
        assert!(content.contains("out"));
        assert!(content.contains("[stderr]"));
        assert!(content.contains("err"));
        assert!(content.contains("[timeout]"));
        assert!(content.contains("[truncated]"));
        assert!(
            content.contains("sessions/sid/out.stdout.out"),
            "截断提示应带上完整输出相对路径：{content}"
        );
    }

    #[test]
    fn approval_records_are_not_projected_and_do_not_break_pairing() {
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo", None),
            Record::approval_pending("c1", "echo", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
            result("c1", "OUT"),
        ];
        let messages = render(&records);
        assert_eq!(
            roles(&messages),
            ["user", "assistant", "tool"],
            "审批记录不得产出消息：{messages:?}"
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(
            messages[2].content.as_deref(),
            Some("OUT"),
            "审批记录不得改变 assistant/tool 配对"
        );
    }

    #[test]
    fn pending_without_result_renders_pending_not_interrupted() {
        // P3：未决 pending 应表达「待批准、尚未执行」，不得伪装成运行中断。
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "rm -rf /", None),
            Record::approval_pending("c1", "rm -rf /", "rm *"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "tool"]);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        let content = messages[2].content.as_deref().unwrap();
        assert!(content.contains("pending"), "应为待批标记：{content}");
        assert!(content.contains("等待批准"), "应说明尚未执行：{content}");
        assert!(
            !content.contains("interrupted"),
            "未决 pending 不得使用中断标记：{content}"
        );
    }

    #[test]
    fn resolved_pending_without_result_is_interrupted() {
        // 已有 approval_resolved 却仍无 tool_result（如结果缺失）→ 不再算待批。
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "rm -rf /", None),
            Record::approval_pending("c1", "rm -rf /", "rm *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Deny),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "tool"]);
        assert!(
            messages[2]
                .content
                .as_deref()
                .unwrap()
                .contains("interrupted"),
            "已决议但缺结果的调用仍是中断：{:?}",
            messages[2].content
        );
    }

    #[test]
    fn pending_with_tool_result_renders_real_output() {
        // 有 pending 但结果已到（如 --approve 后执行完毕）→ 仍以真实输出渲染。
        let records = vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo", None),
            Record::approval_pending("c1", "echo", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
            result("c1", "REAL"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "tool"]);
        assert_eq!(messages[2].content.as_deref(), Some("REAL"));
    }

    #[test]
    fn user_after_ai_starts_new_run() {
        let records = vec![
            Record::user("第一问"),
            Record::ai("m", "h", "答", None),
            Record::user("第二问"),
            notify("补充"),
        ];
        let messages = render(&records);
        assert_eq!(roles(&messages), ["user", "assistant", "user"]);
        assert_eq!(messages[0].content.as_deref(), Some("第一问"));
        assert_eq!(messages[2].content.as_deref(), Some("第二问\n\n补充"));
    }
}
