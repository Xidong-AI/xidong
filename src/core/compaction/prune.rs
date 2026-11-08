//! 工具结果正文的头尾修剪（D53）。
//!
//! 与 `[shell]` 的**执行期截断**不同：这里是压缩触发后对历史里既有的 `tool`
//! 消息再做一次头尾修剪，属渲染期变换、不改记录。规则照搬 DSH
//! `compaction-tool-result-pruner`：只修剪 `tool` 角色，`system` 前缀绝不受影响。

use crate::core::config::CompactionConfig;
use crate::core::message::ChatMessage;

/// 修剪标记：插在保留的头部与尾部之间（照搬 DSH）。
pub const PRUNE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";

/// 修剪预算（Unicode 码点数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneBudgets {
    /// 正文码点数超过此值才修剪。
    pub threshold_chars: usize,
    /// 保留的头部码点数。
    pub head_chars: usize,
    /// 保留的尾部码点数。
    pub tail_chars: usize,
}

impl PruneBudgets {
    /// 由压缩配置派生。
    pub fn from_config(cfg: &CompactionConfig) -> Self {
        Self {
            threshold_chars: cfg.prune_threshold_chars,
            head_chars: cfg.prune_head_chars,
            tail_chars: cfg.prune_tail_chars,
        }
    }
}

/// 修剪一段文本。
///
/// 未超过 `threshold_chars` 个码点时返回 `None`（不修剪）；否则返回
/// `前 head_chars 个码点 + PRUNE_MARKER + 后 tail_chars 个码点`。
///
/// 切片按 Unicode 码点（`char`）进行，绝不拆开 UTF-8 编码；字素簇仍可能被切断
/// （与 DSH 相同的已知限制）。结果必须严格短于原文。
pub fn prune_text(text: &str, budgets: &PruneBudgets) -> Option<String> {
    // 先物化码点序列：后续头部、尾部与长度都基于同一份序列，避免重复迭代，
    // 也保证按码点（而非字节）边界切片。
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total <= budgets.threshold_chars {
        return None;
    }

    // 保留量不得超过原文，否则会越过序列末尾。
    let head_len = budgets.head_chars.min(total);
    let tail_len = budgets.tail_chars.min(total);
    // 预算畸形（head + tail > total）时头尾区间会重叠，尾部必须从头之后开始，
    // 否则同一段字符被保留两次；正常配置下 `total - tail_len` 已不小于 head_len。
    let tail_start = total.saturating_sub(tail_len).max(head_len);

    let mut pruned = String::with_capacity(total + PRUNE_MARKER.len());
    pruned.extend(chars[..head_len].iter());
    pruned.push_str(PRUNE_MARKER);
    pruned.extend(chars[tail_start..].iter());
    Some(pruned)
}

/// 原地修剪所有 `tool` 角色的消息正文，返回被修剪的消息条数。
///
/// 非 `tool` 角色（尤其 `system`）与未超预算的消息保持不变。
pub fn prune_messages(messages: &mut [ChatMessage], budgets: &PruneBudgets) -> usize {
    let mut pruned = 0;
    for message in messages.iter_mut() {
        // 只动工具结果：`system` 前缀与用户/助手内容必须逐字保留。
        if message.role != "tool" {
            continue;
        }
        let Some(content) = message.content.as_ref() else {
            continue;
        };
        if let Some(new_content) = prune_text(content, budgets) {
            message.content = Some(new_content);
            pruned += 1;
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造预算；测试里直接给出码点数，避免依赖配置校验。
    fn budgets(threshold_chars: usize, head_chars: usize, tail_chars: usize) -> PruneBudgets {
        PruneBudgets {
            threshold_chars,
            head_chars,
            tail_chars,
        }
    }

    #[test]
    fn prune_text_below_or_at_threshold_returns_none() {
        let text = "短文本";
        let budget = budgets(3, 2, 1);
        assert_eq!(prune_text(text, &budget), None, "未超阈值的文本不应修剪");
    }

    #[test]
    fn prune_text_above_threshold_keeps_head_tail_and_marker() {
        let text = "abcdefghij";
        let budget = budgets(5, 3, 2);
        let pruned = prune_text(text, &budget).expect("超阈值应修剪");
        assert!(pruned.starts_with("abc"), "应以保留的头部开始");
        assert!(pruned.ends_with("ij"), "应以保留的尾部结束");
        assert!(pruned.contains(PRUNE_MARKER), "头尾之间应插入修剪标记");
    }

    #[test]
    fn prune_text_counts_cjk_by_code_point() {
        // 4 个 CJK 码点；head=2 / tail=1 / threshold=3 时应按码点而非字节切分。
        let text = "中文测试";
        let budget = budgets(3, 2, 1);
        let pruned = prune_text(text, &budget).expect("超阈值应修剪");
        assert_eq!(
            pruned,
            format!("中文{PRUNE_MARKER}试"),
            "CJK 文本应恰好保留头部 2 个、尾部 1 个码点"
        );
    }

    #[test]
    fn prune_text_kept_counts_match_config() {
        let text = "abcdefghij";
        let budget = budgets(5, 3, 2);
        let pruned = prune_text(text, &budget).expect("超阈值应修剪");
        let kept: String = pruned.chars().filter(|c| *c != '\n').collect();
        assert!(kept.starts_with("abc"), "头部保留码点数应与配置一致");
        assert!(kept.ends_with("ij"), "尾部保留码点数应与配置一致");
        assert_eq!(
            pruned.chars().count(),
            3 + PRUNE_MARKER.chars().count() + 2,
            "结果长度应为头部 + 标记 + 尾部"
        );
    }

    #[test]
    fn prune_text_result_is_strictly_shorter() {
        // 原文足够长时，头部 + 标记 + 尾部必定短于原文。
        let text = "x".repeat(100);
        let budget = budgets(20, 10, 10);
        let pruned = prune_text(&text, &budget).expect("超阈值应修剪");
        assert!(
            pruned.chars().count() < text.chars().count(),
            "修剪结果必须严格短于原文"
        );
    }

    #[test]
    fn prune_messages_only_touches_tool_role() {
        let long = "y".repeat(100);
        let mut messages = vec![
            ChatMessage::system(format!("系统前缀{long}")),
            ChatMessage::user(format!("用户内容{long}")),
            ChatMessage::tool_result("call-1".to_string(), long.clone()),
            ChatMessage::assistant(Some(format!("助手{long}")), None, None),
        ];
        let system_before = messages[0].clone();
        let user_before = messages[1].clone();
        let assistant_before = messages[3].clone();

        let pruned = prune_messages(&mut messages, &budgets(20, 10, 10));

        assert_eq!(pruned, 1, "只应修剪 1 条 tool 消息");
        assert_eq!(messages[0], system_before, "system 消息必须逐字不变");
        assert_eq!(messages[1], user_before, "user 消息必须逐字不变");
        assert_eq!(messages[3], assistant_before, "assistant 消息必须逐字不变");
        let tool_content = messages[2].content.as_ref().expect("tool 正文仍在");
        assert!(
            tool_content.contains(PRUNE_MARKER),
            "tool 正文应被修剪并带上标记"
        );
        assert_eq!(
            messages[2].tool_call_id.as_deref(),
            Some("call-1"),
            "tool 消息的其它字段不应改动"
        );
    }

    #[test]
    fn prune_messages_skips_tool_below_threshold() {
        let mut messages = vec![ChatMessage::tool_result(
            "call-1".to_string(),
            "短".to_string(),
        )];
        let before = messages[0].clone();
        let pruned = prune_messages(&mut messages, &budgets(10, 5, 2));
        assert_eq!(pruned, 0, "未超预算的 tool 消息不应计数");
        assert_eq!(messages[0], before, "未超预算的 tool 消息应逐字不变");
    }
}
