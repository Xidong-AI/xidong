//! 阶段 6 不变量检查器：供 e2e/harness 与单测共用的纯查询函数。
//!
//! 两个入口：
//! - [`check_consistency`]：任意时刻都应成立的结构不变量（可对运行中的会话调用）；
//! - [`check_converged`]：收敛态附加不变量（run 结束、`--clear` 之后调用）。
//!
//! 检查器只读会话数据库与文件 inbox/spool，唯一的副作用是 `check_converged` 会按
//! spec 尝试获取并立即释放 `runner.lock`（验证无并发 runner 持有）。复用
//! [`crate::agent`] 的 `orphan_tool_calls`/`unresolved_pending` 逻辑，避免配对语义漂移。

use std::collections::{HashMap, HashSet};

use yaa_msg::Record;
use yaa_store::{InboxPhase, Session};

/// 检测到的不变量违例，逐条人类可读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violations(pub Vec<String>);

impl Violations {
    /// 是否无违例。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 汇总为单行描述（无违例时为「无违例」）。
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            "无违例".to_string()
        } else {
            self.0.join("；")
        }
    }
}

/// 任意时刻都应成立的结构不变量。
///
/// - 无重复记录 id；
/// - `seq` 严格递增；
/// - 同一 `call_id` 至多一条成功（`code == 0`）`tool_result`；
/// - 每条 `approval_resolved` 都有同 `call_id` 的 `approval_pending`；
/// - 每个 `tool_call` 的 `call_id` 唯一。
pub fn check_consistency(session: &Session) -> Violations {
    let Ok((rows, _)) = yaa_store::read_after(session, 0) else {
        return Violations(vec!["读取会话失败".to_string()]);
    };
    Violations(consistency_violations(&rows))
}

/// 收敛态附加不变量：在 [`check_consistency`] 之上，要求无孤儿 `tool_call`、无未决
/// `approval_pending`，`inbox/{pending,init}` 与 `spool` 无待消费 `.json`，且
/// `runner.lock` 可被独占获取（获取后立即释放，验证没有并发 runner）。
pub fn check_converged(session: &Session) -> Violations {
    let Ok((rows, _)) = yaa_store::read_after(session, 0) else {
        return Violations(vec!["读取会话失败".to_string()]);
    };
    let mut violations = consistency_violations(&rows);
    let records: Vec<Record> = rows.into_iter().map(|(_, record)| record).collect();

    for call_id in crate::agent::orphan_tool_calls(&records) {
        violations.push(format!("孤儿 tool_call（无配对结果）：call_id={call_id}"));
    }
    for call_id in crate::agent::unresolved_pending_call_ids(&records) {
        violations.push(format!("未决 approval_pending：call_id={call_id}"));
    }

    for phase in [InboxPhase::Pending, InboxPhase::Init] {
        match yaa_store::count_inbox(session, phase) {
            Ok(0) => {}
            Ok(count) => violations.push(format!(
                "inbox/{} 残留 {count} 条待消费消息",
                phase.dir_name()
            )),
            Err(err) => violations.push(format!("统计 inbox/{} 失败：{err:#}", phase.dir_name())),
        }
    }
    match yaa_store::count_spool(session) {
        Ok(0) => {}
        Ok(count) => violations.push(format!("spool 残留 {count} 条待补投消息")),
        Err(err) => violations.push(format!("统计 spool 失败：{err:#}")),
    }

    match yaa_store::FileLock::try_exclusive(&session.runner_lock_path()) {
        // 获取成功即释放：确认没有别的 runner 持有。
        Ok(Some(lock)) => drop(lock),
        Ok(None) => violations.push("runner.lock 已被占用（存在并发 runner）".to_string()),
        Err(err) => violations.push(format!("无法获取 runner.lock：{err:#}")),
    }

    Violations(violations)
}

/// `(seq, record)` 序列上的结构违例。
fn consistency_violations(rows: &[(i64, Record)]) -> Vec<String> {
    let mut violations = Vec::new();

    let mut seen_ids = HashSet::new();
    for (_, record) in rows {
        if !seen_ids.insert(record.id()) {
            violations.push(format!("重复记录 id：{}", record.id()));
        }
    }

    for window in rows.windows(2) {
        if window[1].0 <= window[0].0 {
            violations.push(format!("seq 未严格递增：{} → {}", window[0].0, window[1].0));
        }
    }

    let mut successful_results: HashMap<&str, usize> = HashMap::new();
    for (_, record) in rows {
        if let Record::ToolResult(result) = record
            && result.code == 0
        {
            *successful_results
                .entry(result.call_id.as_str())
                .or_insert(0) += 1;
        }
    }
    let mut successful_results: Vec<_> = successful_results.into_iter().collect();
    successful_results.sort_unstable();
    for (call_id, count) in successful_results {
        if count > 1 {
            violations.push(format!(
                "同一 call_id 有 {count} 条成功 tool_result：call_id={call_id}"
            ));
        }
    }

    let pending_ids: HashSet<&str> = rows
        .iter()
        .filter_map(|(_, record)| match record {
            Record::ApprovalPending(pending) => Some(pending.call_id.as_str()),
            _ => None,
        })
        .collect();
    for (_, record) in rows {
        if let Record::ApprovalResolved(resolved) = record
            && !pending_ids.contains(resolved.call_id.as_str())
        {
            violations.push(format!(
                "approval_resolved 无对应 approval_pending：call_id={}",
                resolved.call_id
            ));
        }
    }

    let mut seen_calls = HashSet::new();
    for (_, record) in rows {
        if let Record::ToolCall(call) = record
            && !seen_calls.insert(call.call_id.as_str())
        {
            violations.push(format!("重复 tool_call call_id：{}", call.call_id));
        }
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.join("session.db"))
    }

    fn seed(session: &Session, records: &[Record]) {
        yaa_store::append(session, records).unwrap();
    }

    /// 一段结构完整、已收敛的历史：ai 工具调用有结果，审批 pending 已 resolved。
    fn converged_history() -> Vec<Record> {
        vec![
            Record::user("你好"),
            Record::ai("m", "h", "", None),
            Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
            Record::tool_result("c1", "hi", "", 0, false, false, None),
            Record::approval_pending("c2", "rm x", "rm *"),
            Record::approval_resolved("c2", yaa_msg::ApprovalDecision::Deny),
            Record::user("再见"),
        ]
    }

    #[test]
    fn converged_history_reports_no_violations() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(&session, &converged_history());

        assert!(
            check_consistency(&session).is_empty(),
            "{}",
            check_consistency(&session).describe()
        );
        let converged = check_converged(&session);
        assert!(converged.is_empty(), "{}", converged.describe());
    }

    #[test]
    fn empty_session_is_consistent_and_converged() {
        let dir = tempfile::tempdir().unwrap();
        // 先物化会话目录（runner.lock 需要父目录存在）。
        let session = session_in(dir.path());
        seed(&session, &[Record::user("seed")]);

        assert!(check_consistency(&session).is_empty());
        assert!(check_converged(&session).is_empty());
    }

    #[test]
    fn orphan_tool_call_is_only_a_convergence_violation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[
                Record::user("你好"),
                Record::ai("m", "h", "", None),
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
            ],
        );

        // 结构上合法（未决 pending 允许存在），但收敛态禁止孤儿。
        assert!(check_consistency(&session).is_empty());
        let converged = check_converged(&session);
        assert!(!converged.is_empty());
        assert!(
            converged.describe().contains("孤儿"),
            "{}",
            converged.describe()
        );
    }

    #[test]
    fn duplicate_successful_result_is_a_consistency_violation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
                Record::tool_result("c1", "hi", "", 0, false, false, None),
                Record::tool_result("c1", "hi again", "", 0, false, false, None),
            ],
        );

        let violations = check_consistency(&session);
        assert!(!violations.is_empty());
        assert!(
            violations.describe().contains("成功 tool_result"),
            "{}",
            violations.describe()
        );
    }

    #[test]
    fn failed_results_do_not_count_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "false", None),
                Record::tool_result("c1", "", "boom", 1, false, false, None),
                Record::tool_result("c1", "", "boom again", 2, false, false, None),
            ],
        );

        assert!(
            check_consistency(&session).is_empty(),
            "非零退出码的结果不计入成功结果"
        );
    }

    #[test]
    fn unresolved_pending_is_a_convergence_violation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[
                Record::user("你好"),
                Record::ai("m", "h", "", None),
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "rm x", None),
                Record::approval_pending("c1", "rm x", "rm *"),
            ],
        );

        assert!(check_consistency(&session).is_empty());
        let converged = check_converged(&session);
        assert!(
            converged.describe().contains("未决 approval_pending"),
            "{}",
            converged.describe()
        );
    }

    #[test]
    fn resolved_without_pending_is_a_consistency_violation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[Record::approval_resolved(
                "ghost",
                yaa_msg::ApprovalDecision::Approve,
            )],
        );

        let violations = check_consistency(&session);
        assert!(
            violations.describe().contains("无对应 approval_pending"),
            "{}",
            violations.describe()
        );
    }

    #[test]
    fn held_runner_lock_blocks_convergence() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(&session, &[Record::user("seed")]);
        let held = yaa_store::FileLock::try_exclusive(&session.runner_lock_path())
            .unwrap()
            .expect("测试进程应能持锁");

        let converged = check_converged(&session);
        assert!(
            converged.describe().contains("runner.lock"),
            "{}",
            converged.describe()
        );
        drop(held);
        assert!(check_converged(&session).is_empty());
    }

    #[test]
    fn duplicate_tool_call_id_is_a_consistency_violation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(
            &session,
            &[
                Record::ai("m", "h", "", None),
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "dup", "echo 1", None),
                Record::tool_call(yaa_msg::ToolOrigin::Ai, "dup", "echo 2", None),
            ],
        );

        let violations = check_consistency(&session);
        assert!(
            violations.describe().contains("重复 tool_call call_id"),
            "{}",
            violations.describe()
        );
    }

    #[test]
    fn residual_inbox_blocks_convergence() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        seed(&session, &[Record::user("seed")]);
        yaa_store::publish_inbox(&session, InboxPhase::Pending, &Record::user("未消费")).unwrap();

        let converged = check_converged(&session);
        assert!(
            converged.describe().contains("inbox/pending"),
            "{}",
            converged.describe()
        );
    }
}
