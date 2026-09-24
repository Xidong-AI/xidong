//! 固定密度 token 估算（D52）。
//!
//! 按 `chars_per_token`（默认 4）对文本字符数取上取整；已知低估 CJK（见
//! `docs/plan.md` 已知限制第 5 条）。估算只用于触发判断与「摘要是否更小」的
//! 收敛校验，不参与任何正确性决策。

use yaa_msg::Record;

use crate::message::ChatMessage;

/// token 估算器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimator {
    chars_per_token: usize,
}

impl Estimator {
    /// 由配置的 `chars_per_token` 构造；`0` 视为 `1`（避免除零）。
    pub fn new(chars_per_token: u32) -> Self {
        Self {
            chars_per_token: chars_per_token.max(1) as usize,
        }
    }

    /// 估算一段文本的 token 数：`ceil(字符数 / chars_per_token)`。
    pub fn estimate_text(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        text.chars().count().div_ceil(self.chars_per_token)
    }

    /// 估算一条聊天消息：`content` + 工具调用参数 + `reasoning`。
    ///
    /// 不含角色框定与 JSON 结构开销（对齐 DSH 的粗略密度启发式）。
    pub fn estimate_message(&self, message: &ChatMessage) -> usize {
        let mut chars = 0usize;
        if let Some(content) = &message.content {
            chars += content.chars().count();
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                chars += call.name.chars().count();
                chars += call.arguments.chars().count();
            }
        }
        if let Some(reasoning) = &message.reasoning {
            chars += reasoning.chars().count();
        }
        chars.div_ceil(self.chars_per_token)
    }

    /// 估算一组消息的 token 总和。
    pub fn estimate_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|message| self.estimate_message(message))
            .sum()
    }

    /// 估算一条记录的 token：按其会投影出的文本字段计。
    ///
    /// - `user`：`content`；
    /// - `ai`：`content` + `reasoning`（工具参数由独立的 `tool_call` 记录计）；
    /// - `tool_call`：`command` + `description`；
    /// - `tool_result`：`stdout` + `stderr`；
    /// - `system`：`command` + `content`；
    /// - 审批记录不投影，计 `0`。
    pub fn estimate_record(&self, record: &Record) -> usize {
        let chars = match record {
            Record::User(user) => user.content.chars().count(),
            Record::Ai(ai) => {
                ai.content.chars().count()
                    + ai.reasoning.as_deref().map_or(0, |text| text.chars().count())
            }
            Record::ToolCall(call) => {
                call.command.chars().count()
                    + call.description.as_deref().map_or(0, |text| text.chars().count())
            }
            Record::ToolResult(result) => {
                result.stdout.chars().count() + result.stderr.chars().count()
            }
            Record::System(system) => {
                system.content.chars().count()
                    + system.command.as_deref().map_or(0, |text| text.chars().count())
            }
            Record::ApprovalPending(_) | Record::ApprovalResolved(_) => 0,
        };
        if chars == 0 {
            0
        } else {
            chars.div_ceil(self.chars_per_token)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rounds_up() {
        let estimator = Estimator::new(4);
        assert_eq!(estimator.estimate_text(""), 0);
        assert_eq!(estimator.estimate_text("abcd"), 1);
        assert_eq!(estimator.estimate_text("abcde"), 2);
    }

    #[test]
    fn cjk_counts_by_chars_not_bytes() {
        // 4 个 CJK 字符按 4 字符/token 记 1 token（字节数会误记 3）。
        let estimator = Estimator::new(4);
        assert_eq!(estimator.estimate_text("你好世界"), 1);
    }

    #[test]
    fn record_fields_are_summed() {
        let estimator = Estimator::new(1);
        let record = Record::ai("m", "h", "abcd", Some("ef".into()));
        assert_eq!(estimator.estimate_record(&record), 6);
        let call = Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo", Some("hi".into()));
        assert_eq!(estimator.estimate_record(&call), 6);
    }

    #[test]
    fn approval_records_are_free() {
        let estimator = Estimator::new(4);
        let record = Record::approval_pending("c1", "rm -rf /", "rm *");
        assert_eq!(estimator.estimate_record(&record), 0);
    }
}
