//! token 估算（D52）。
//!
//! 采用二分类启发式：ASCII 码点按 `chars_per_token`（默认 4）密度折算，非 ASCII
//! 码点（CJK、emoji 等）每码点计 1 token——修正了旧版固定密度对 CJK 约 4 倍的
//! 系统性低估。估算只用于触发判断与「摘要是否更小」的收敛校验，不参与任何正确性
//! 决策。
//!
//! 各估算方法接受可选的 [`PrecomputedMap`]：精确命中某段文本时以其值为准，未命中
//! 则回退启发式。刀 1 阶段一律传 `None`，预计算表由后续阶段填充。

use std::collections::HashMap;

use crate::msg::Record;

use crate::core::message::ChatMessage;

/// 文本 → 预计算 token 数的覆盖表，按精确字符串相等匹配。
pub type PrecomputedMap = HashMap<String, usize>;

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

    /// 估算一段文本的 token 数：ASCII 码点按 `chars_per_token` 取上整，非 ASCII
    /// 码点每码点计 1。空串自然得 0。
    pub fn estimate_text(&self, text: &str) -> usize {
        let mut ascii = 0usize;
        let mut wide = 0usize;
        for c in text.chars() {
            if c.is_ascii() {
                ascii += 1;
            } else {
                wide += 1;
            }
        }
        ascii.div_ceil(self.chars_per_token) + wide
    }

    /// 若 `pre` 精确命中 `text` 则返回其值，否则回退 [`Self::estimate_text`]。
    pub fn estimate_text_with(&self, text: &str, pre: Option<&PrecomputedMap>) -> usize {
        if let Some(value) = pre.and_then(|map| map.get(text)) {
            return *value;
        }
        self.estimate_text(text)
    }

    /// 估算一条聊天消息：`content`、每个工具调用的 `name`/`arguments` 与
    /// `reasoning` 各自独立折算后求和。
    ///
    /// 不含角色框定与 JSON 结构开销（对齐 DSH 的粗略密度启发式）。
    pub fn estimate_message(&self, message: &ChatMessage, pre: Option<&PrecomputedMap>) -> usize {
        let mut tokens = 0usize;
        if let Some(content) = &message.content {
            tokens += self.estimate_text_with(content, pre);
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                tokens += self.estimate_text_with(&call.name, pre);
                tokens += self.estimate_text_with(&call.arguments, pre);
            }
        }
        if let Some(reasoning) = &message.reasoning {
            tokens += self.estimate_text_with(reasoning, pre);
        }
        tokens
    }

    /// 估算一组消息的 token 总和。
    pub fn estimate_messages(
        &self,
        messages: &[ChatMessage],
        pre: Option<&PrecomputedMap>,
    ) -> usize {
        messages
            .iter()
            .map(|message| self.estimate_message(message, pre))
            .sum()
    }

    /// 估算一条记录的 token：按其会投影出的文本字段逐字段折算后求和。
    ///
    /// - `user`：`content`；
    /// - `ai`：`content` + `reasoning`（工具参数由独立的 `tool_call` 记录计）；
    /// - `tool_call`：`command` + `description`；
    /// - `tool_result`：`stdout` + `stderr`；
    /// - `system`：`command` + `content`；
    /// - 审批记录不投影，计 `0`。
    pub fn estimate_record(&self, record: &Record, pre: Option<&PrecomputedMap>) -> usize {
        match record {
            Record::User(user) => self.estimate_text_with(&user.content, pre),
            Record::Ai(ai) => {
                let mut tokens = self.estimate_text_with(&ai.content, pre);
                if let Some(reasoning) = ai.reasoning.as_deref() {
                    tokens += self.estimate_text_with(reasoning, pre);
                }
                tokens
            }
            Record::ToolCall(call) => {
                let mut tokens = self.estimate_text_with(&call.command, pre);
                if let Some(description) = call.description.as_deref() {
                    tokens += self.estimate_text_with(description, pre);
                }
                tokens
            }
            Record::ToolResult(result) => {
                self.estimate_text_with(&result.stdout, pre)
                    + self.estimate_text_with(&result.stderr, pre)
            }
            Record::System(system) => {
                let mut tokens = self.estimate_text_with(&system.content, pre);
                if let Some(command) = system.command.as_deref() {
                    tokens += self.estimate_text_with(command, pre);
                }
                tokens
            }
            Record::ApprovalPending(_) | Record::ApprovalResolved(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::message::AssistantToolCall;

    #[test]
    fn text_rounds_up() {
        let estimator = Estimator::new(4);
        assert_eq!(estimator.estimate_text(""), 0);
        assert_eq!(estimator.estimate_text("abcd"), 1);
        assert_eq!(estimator.estimate_text("abcde"), 2);
    }

    #[test]
    fn cjk_one_token_per_codepoint() {
        // 二分类：4 个 CJK 码点各计 1 token（旧固定密度会误记 1）。
        let estimator = Estimator::new(4);
        assert_eq!(estimator.estimate_text("你好世界"), 4);
    }

    #[test]
    fn mixed_ascii_and_cjk() {
        let estimator = Estimator::new(4);
        // ASCII 部分（含空格）ceil(4/4) = 1，非 ASCII 2 码点计 2，共 3。
        assert_eq!(estimator.estimate_text("abc 你好"), 3);
    }

    #[test]
    fn multi_codepoint_emoji_counts_per_codepoint() {
        let estimator = Estimator::new(4);
        // 「👨‍👩‍👧」由 3 个 emoji 与 2 个 ZWJ 共 5 个码点组成，均非 ASCII，各计 1。
        let family = "👨‍👩‍👧";
        assert_eq!(family.chars().count(), 5);
        assert_eq!(estimator.estimate_text(family), 5);
    }

    #[test]
    fn precomputed_map_overrides_and_falls_back() {
        let estimator = Estimator::new(4);
        let mut pre = PrecomputedMap::new();
        pre.insert("你好".to_string(), 42);
        // 命中覆盖表。
        assert_eq!(estimator.estimate_text_with("你好", Some(&pre)), 42);
        // 未命中回退启发式（2 个非 ASCII 码点）。
        assert_eq!(estimator.estimate_text_with("世界", Some(&pre)), 2);
        // 无覆盖表时同样走启发式。
        assert_eq!(estimator.estimate_text_with("abcd", None), 1);
    }

    #[test]
    fn message_fields_are_ceiled_individually_then_summed() {
        let estimator = Estimator::new(4);
        let message = ChatMessage::assistant(
            Some("ab".into()),
            Some(vec![AssistantToolCall {
                id: "c1".to_string(),
                name: "c".to_string(),
                arguments: "d".to_string(),
            }]),
            Some("e".into()),
        );
        // 各字段独立 ceil(1/4) = 1，共 4；若整体求和再 ceil 会得 ceil(5/4) = 2。
        assert_eq!(estimator.estimate_message(&message, None), 4);
    }

    #[test]
    fn record_fields_are_summed() {
        let estimator = Estimator::new(1);
        let record = Record::ai("m", "h", "abcd", Some("ef".into()));
        assert_eq!(estimator.estimate_record(&record, None), 6);
        let call = Record::tool_call(crate::msg::ToolOrigin::Ai, "c1", "echo", Some("hi".into()));
        assert_eq!(estimator.estimate_record(&call, None), 6);
    }

    #[test]
    fn approval_records_are_free() {
        let estimator = Estimator::new(4);
        let record = Record::approval_pending("c1", "rm -rf /", "rm *");
        assert_eq!(estimator.estimate_record(&record, None), 0);
    }
}
