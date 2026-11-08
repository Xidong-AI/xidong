//! 默认摘要器：检查点框定与摘要调用输入（D54）。
//!
//! 指令文本照搬 DSH `compaction-basic` 的 `COMPACTION_INSTRUCTION` 与
//! `CHECKPOINT_PREAMBLE`：摘要指令作为**最后一条 user 消息**追加在被遮蔽对话之后，
//! 替换消息以固定标签框定，使模型把摘要当作既有背景而非新指令。

use crate::core::message::ChatMessage;

/// 摘要正文的开标签。
pub const SUMMARY_OPEN_TAG: &str = "<compacted-summary>";
/// 摘要正文的闭标签。
pub const SUMMARY_CLOSE_TAG: &str = "</compacted-summary>";

/// 替换消息的前导语：让模型把检查点当作既有背景，直接继续任务。
pub const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// 摘要指令（追加为摘要调用的最后一条 `user` 消息）。
pub const COMPACTION_INSTRUCTION: &str = concat!(
    "You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.\n",
    "\n",
    "Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write \"(none)\" for an empty section — never drop a section.\n",
    "\n",
    "## Primary Request and Intent\n",
    "- [the user's original and evolving goals; quote verbatim where the exact wording matters]\n",
    "\n",
    "## Key Technical Concepts\n",
    "- [technologies, frameworks, patterns, and conventions in play]\n",
    "\n",
    "## Files and Code\n",
    "- [exact path: why it matters, key changes or snippets]\n",
    "\n",
    "## Errors and Fixes\n",
    "- [error: how it was resolved, plus any related user feedback]\n",
    "\n",
    "## Pending Jobs\n",
    "- [explicitly requested work not yet completed]\n",
    "\n",
    "## Current Work\n",
    "- [precisely what was in progress at this checkpoint]\n",
    "\n",
    "## Next Step\n",
    "- [the single next action, directly in line with the most recent request, or \"(none)\"]\n",
    "\n",
    "## Critical Context\n",
    "- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]\n",
    "\n",
    "Rules:\n",
    "- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.\n",
    "- Capture user feedback and explicit instructions faithfully, especially corrections.\n",
    "- Do NOT mention this summarization request or that the context was compacted.\n",
    "- Output only the checkpoint text: do not call any tool or take any other action.\n",
    "- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure."
);

/// 由摘要正文构造替换消息（`user` 角色，含前导语与标签）。
pub fn checkpoint_message(summary: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "{CHECKPOINT_PREAMBLE}\n\n{SUMMARY_OPEN_TAG}{summary}{SUMMARY_CLOSE_TAG}"
    ))
}

/// 构造摘要调用输入：可选稳定前缀 + 被遮蔽消息 + 指令。
///
/// 稳定前缀（系统提示词）只作摘要器的语境，不参与遮蔽；指令恒为最后一条消息。
pub fn summary_input(prefix: Option<&ChatMessage>, shadowed: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(shadowed.len() + 2);
    if let Some(prefix) = prefix {
        messages.push(prefix.clone());
    }
    messages.extend_from_slice(shadowed);
    messages.push(ChatMessage::user(COMPACTION_INSTRUCTION));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_message_is_user_with_tags_and_preamble() {
        let message = checkpoint_message("## Primary Request\n- 做某事");
        assert_eq!(message.role, "user");
        let content = message.content.as_deref().unwrap();
        assert!(content.starts_with(CHECKPOINT_PREAMBLE));
        assert!(content.contains("<compacted-summary>"));
        assert!(content.ends_with("</compacted-summary>"));
        assert!(content.contains("做某事"));
    }

    #[test]
    fn summary_input_orders_prefix_shadowed_instruction() {
        let prefix = ChatMessage::system("系统提示词");
        let shadowed = vec![ChatMessage::user("第一问"), ChatMessage::user("第二问")];
        let messages = summary_input(Some(&prefix), &shadowed);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].content.as_deref(), Some("第一问"));
        assert_eq!(messages[2].content.as_deref(), Some("第二问"));
        assert_eq!(messages[3].content.as_deref(), Some(COMPACTION_INSTRUCTION));
    }

    #[test]
    fn summary_input_without_prefix_still_appends_instruction() {
        let messages = summary_input(None, &[ChatMessage::user("问题")]);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].content.as_deref(), Some(COMPACTION_INSTRUCTION));
    }

    #[test]
    fn instruction_keeps_all_sections_in_order() {
        let required = [
            "## Primary Request and Intent",
            "## Key Technical Concepts",
            "## Files and Code",
            "## Errors and Fixes",
            "## Pending Jobs",
            "## Current Work",
            "## Next Step",
            "## Critical Context",
        ];
        let mut cursor = 0;
        for section in required {
            let found = COMPACTION_INSTRUCTION[cursor..]
                .find(section)
                .map(|offset| cursor + offset)
                .unwrap_or_else(|| panic!("指令缺少段落 {section}"));
            assert!(found >= cursor, "段落顺序错乱：{section}");
            cursor = found + section.len();
        }
    }
}
