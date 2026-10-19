//! 可压缩区间的选择（照搬 DSH `compaction-basic` 的边界规则）。
//!
//! 规则（见 `docs/plan.md` 阶段 3）：稳定 `system` 头永不进入区间；从尾部累计
//! token 到保留预算；切点再向头部回退到「步骤边界」，绝不切分一个助手步骤
//! （`assistant.tool_calls` 与其配对 `tool` 结果必须同进同出）。

use yaa_msg::{Record, ToolOrigin};

/// 记录是否为「步骤边界」，即保留区可否从它开始。
///
/// - `user` / `ai` / `system`：**是**（新的用户轮次、新的助手步骤或一条注入）；
/// - `tool_call` / `tool_result` / 审批记录：**否**（依附于其助手步骤，若作为保留区
///   起点会让保留区以孤儿工具消息开头）。
pub fn is_step_boundary(record: &Record) -> bool {
    matches!(record, Record::User(_) | Record::Ai(_) | Record::System(_))
}

/// 选择可遮蔽的记录下标区间（含端点）。
///
/// - `tokens[i]` 为第 `i` 条记录的 token 估算（与 `records` 等长）；
/// - `boundaries[i]` 为第 `i` 条记录是否为步骤边界（与 `records` 等长）；
/// - `first_idx` 为稳定前缀长度（首条 `user` 之前的记录数），永不进入区间；
/// - `retain_tokens` 为逐字保留的近期尾部 token 预算。
///
/// 返回 `(start, end)`，其中 `start` 恒等于 `first_idx`。返回 `None` 当：
/// `tokens`/`boundaries` 长度不等或与 `first_idx >= len`；尾部预算已覆盖到
/// `first_idx` 以内；或回退后无可遮蔽记录。
///
/// 实现（照搬 DSH `selectCompactableRange`）：从 `len - 1` 向 `first_idx` 累计
/// `tokens`，达到 `retain_tokens` 时的下标即初始 `keep_from`；再令
/// `keep_from` 逐一下退，直到 `keep_from <= first_idx` 或 `boundaries[keep_from]`
/// 为真；最终区间为 `(first_idx, keep_from - 1)`。
pub fn select_range(
    tokens: &[usize],
    boundaries: &[bool],
    first_idx: usize,
    retain_tokens: usize,
) -> Option<(usize, usize)> {
    // 两份平行数组必须等长；空历史或稳定前缀已越界时无区间可选。
    if tokens.len() != boundaries.len() {
        return None;
    }
    if tokens.is_empty() || first_idx >= tokens.len() {
        return None;
    }

    // 从尾部向前累计 token，直到覆盖 retain_tokens 预算；`keep_from` 是逐字保留
    // 尾部的起点，也是候选切点。一路累加到 first_idx 都未达标时它停在 first_idx。
    let mut accumulated = 0usize;
    let mut keep_from = first_idx;
    let mut i = tokens.len();
    while i > first_idx {
        i -= 1;
        accumulated = accumulated.saturating_add(tokens[i]);
        keep_from = i;
        if accumulated >= retain_tokens {
            break;
        }
    }

    // 预算已覆盖到稳定前缀以内（甚至更靠前），没有可遮蔽的近期记录。
    if keep_from <= first_idx {
        return None;
    }

    // 切点必须落在步骤边界：向头部回退，避免保留区以孤儿工具/审批消息开头。
    while keep_from > first_idx && !boundaries[keep_from] {
        keep_from -= 1;
    }

    // 回退一路跌回 first_idx，说明整个可压缩段都属于同一个助手步骤，无法安全切分。
    if keep_from <= first_idx {
        return None;
    }

    // 区间含端点：保留区自 keep_from 起，可遮蔽区间止于 keep_from - 1。
    Some((first_idx, keep_from - 1))
}

/// 便捷包装：由记录切片构造 `boundaries` 并调用 [`select_range`]。
pub fn select_range_for(
    records: &[Record],
    tokens: &[usize],
    first_idx: usize,
    retain_tokens: usize,
) -> Option<(usize, usize)> {
    let boundaries: Vec<bool> = records.iter().map(is_step_boundary).collect();
    select_range(tokens, &boundaries, first_idx, retain_tokens)
}

/// `tool_call(origin=ai)` 的便捷判定（供调用方构造 `boundaries` 时复用）。
pub fn is_ai_tool_call(record: &Record) -> bool {
    matches!(record, Record::ToolCall(call) if call.origin == ToolOrigin::Ai)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaa_msg::ApprovalDecision;

    #[test]
    fn select_range_rejects_length_mismatch() {
        let tokens = [1usize, 2, 3];
        let boundaries = [true, true];
        assert_eq!(
            select_range(&tokens, &boundaries, 0, 0),
            None,
            "tokens 与 boundaries 长度不等时应返回 None"
        );
    }

    #[test]
    fn select_range_rejects_empty_or_out_of_bounds_first_idx() {
        assert_eq!(select_range(&[], &[], 0, 0), None, "空历史应返回 None");
        let tokens = [1usize, 2];
        let boundaries = [true, true];
        assert_eq!(
            select_range(&tokens, &boundaries, 2, 0),
            None,
            "first_idx 等于长度时应返回 None"
        );
        assert_eq!(
            select_range(&tokens, &boundaries, 5, 0),
            None,
            "first_idx 越界时应返回 None"
        );
    }

    #[test]
    fn select_range_rejects_when_budget_reaches_first_idx() {
        let tokens = [10usize, 20, 30];
        let boundaries = [true, true, true];
        assert_eq!(
            select_range(&tokens, &boundaries, 0, 1000),
            None,
            "尾部预算覆盖到 first_idx 以内时应返回 None"
        );
    }

    #[test]
    fn select_range_returns_interval_when_cut_is_boundary() {
        // 从尾部累计：index 4=1、3=2、2=3 恰好达到预算 3，初选 keep_from = 2。
        let tokens = [1usize, 1, 1, 1, 1];
        let boundaries = [true, false, true, false, false];
        assert_eq!(
            select_range(&tokens, &boundaries, 0, 3),
            Some((0, 1)),
            "切点在边界上时应直接返回 (first_idx, keep_from - 1)"
        );
    }

    #[test]
    fn select_range_backtracks_cut_to_step_boundary() {
        // 初选 keep_from = 2，但 boundaries[2] 非边界，需回退到 index 1。
        let tokens = [1usize, 1, 1, 1, 1];
        let boundaries = [true, true, false, false, false];
        assert_eq!(
            select_range(&tokens, &boundaries, 0, 3),
            Some((0, 0)),
            "切点落在非边界时应向头部回退到最近的步骤边界"
        );
    }

    #[test]
    fn select_range_returns_none_when_backtrack_falls_to_first_idx() {
        // 初选 keep_from = 2，回退到 index 1、0 都非边界，跌至 first_idx。
        let tokens = [1usize, 1, 1, 1, 1];
        let boundaries = [true, false, false, false, false];
        assert_eq!(
            select_range(&tokens, &boundaries, 0, 3),
            None,
            "回退跌到 first_idx 时应返回 None"
        );
    }

    #[test]
    fn select_range_keeps_stable_prefix_out_of_interval() {
        // first_idx = 1：稳定前缀不计入累计，切点回退也止于 first_idx。
        let tokens = [100usize, 1, 1, 1];
        let boundaries = [true, true, true, true];
        assert_eq!(
            select_range(&tokens, &boundaries, 1, 2),
            Some((1, 1)),
            "区间起点应恒为 first_idx，且不含稳定前缀"
        );
    }

    #[test]
    fn is_step_boundary_accepts_user_ai_system() {
        let user = Record::user("你好");
        let ai = Record::ai("deepseek-chat", "hash", "回复", None);
        let system = Record::system("notify", "内容", None, None);
        assert!(is_step_boundary(&user), "user 应为步骤边界");
        assert!(is_step_boundary(&ai), "ai 应为步骤边界");
        assert!(is_step_boundary(&system), "system 应为步骤边界");
    }

    #[test]
    fn is_step_boundary_rejects_tool_and_approval_records() {
        let tool_call = Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None);
        let tool_result = Record::tool_result("call_1", "hi", "", 0, false, false, None);
        let pending = Record::approval_pending("call_1", "rm -rf /", "rm *");
        let resolved = Record::approval_resolved("call_1", ApprovalDecision::Approve);
        assert!(!is_step_boundary(&tool_call), "tool_call 不应为步骤边界");
        assert!(
            !is_step_boundary(&tool_result),
            "tool_result 不应为步骤边界"
        );
        assert!(
            !is_step_boundary(&pending),
            "approval_pending 不应为步骤边界"
        );
        assert!(
            !is_step_boundary(&resolved),
            "approval_resolved 不应为步骤边界"
        );
    }

    #[test]
    fn select_range_for_keeps_whole_step_together() {
        // records: [ai, tool_call, tool_result, user]，tokens 均 5，预算 5。
        // 尾部累计恰在 index 3 达标，且 index 3 是边界 → 可遮蔽 [0, 2]（整个助手步骤）。
        let records = [
            Record::ai("m", "h", "调用工具", None),
            Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None),
            Record::tool_result("call_1", "hi", "", 0, false, false, None),
            Record::user("下一个问题"),
        ];
        let tokens = [5usize; 4];
        assert_eq!(
            select_range_for(&records, &tokens, 0, 5),
            Some((0, 2)),
            "应遮蔽完整助手步骤，保留区从下一条 user 开始"
        );
    }

    #[test]
    fn select_range_for_refuses_to_orphan_tool_messages() {
        // records: [ai, tool_call, tool_result]，预算落在 tool_result 上。
        // 切点回退到 tool_call 仍非边界，最终跌至 first_idx → 无区间。
        let records = [
            Record::ai("m", "h", "调用工具", None),
            Record::tool_call(ToolOrigin::Ai, "call_1", "echo hi", None),
            Record::tool_result("call_1", "hi", "", 0, false, false, None),
        ];
        let tokens = [10usize; 3];
        assert_eq!(
            select_range_for(&records, &tokens, 0, 10),
            None,
            "切点仅能落在助手步骤内部时，应放弃压缩以避免保留孤儿工具消息"
        );
    }
}
