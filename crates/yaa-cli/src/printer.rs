//! 展示条目到 stderr 的渲染。
//!
//! stdout/stderr 策略完全在 cli：`yaa-core` 只投影 [`DisplayItem`]（D15/D18）。
//!
//! - 默认：把过程记录成「块」，块与块之间用单独一行 `---` 分隔（第一个块前不打印），
//!   最终助手文本只走 stdout；
//! - `--verbose`：在默认之上追加模型思考、命令输出与注入内容全文；
//! - `--quiet`：抑制全部过程输出，但**致命错误与运维警告仍打印到 stderr**
//!   （否则用户看不到失败或配置回退的原因）；待批清单亦豁免——退出码 6 是本次运行的
//!   唯一结论，用户必须知道发生了什么。

use std::sync::atomic::{AtomicBool, Ordering};

use yaa_core::display::{DisplayItem, Sink};

/// 按冗长度把展示条目写到 stderr。
pub struct StderrSink {
    verbose: bool,
    quiet: bool,
    /// 是否已经开过第一个过程块；据此决定后续块前是否打印 `---` 分隔线。
    block_started: AtomicBool,
}

impl StderrSink {
    /// 构造一个 stderr 展示接收器。
    pub fn new(verbose: bool, quiet: bool) -> Self {
        Self {
            verbose,
            quiet,
            block_started: AtomicBool::new(false),
        }
    }

    /// 开始一个过程块：非首块前打印空行与 `---` 分隔线（贴合 `docs/design.md` 示例）。
    fn start_block(&self) {
        if self.block_started.swap(true, Ordering::SeqCst) {
            eprintln!("\n---");
        }
    }

    /// 过程记录收尾：若本次运行确实开过块，则在最终回复前补一条空行与 `---` 分隔线。
    ///
    /// 无任何过程块（例如注入全部命中缓存、空会话之外没有新记录）时不打印；
    /// `--quiet` 下 stderr 保持干净。
    pub fn finish_transcript(&self) {
        if !self.quiet && self.block_started.load(Ordering::SeqCst) {
            eprintln!("\n---");
        }
    }

    /// 该条目是否不受 `--quiet` 抑制。
    ///
    /// `Error`/`Notice` 是诊断信息；`ApprovalPending` 是本次运行的唯一结论（退出码 6），
    /// 三者即便在 `--quiet` 下也必须可见。其余过程输出一律抑制。
    fn visible_in_quiet(item: &DisplayItem) -> bool {
        matches!(
            item,
            DisplayItem::Error(_) | DisplayItem::Notice(_) | DisplayItem::ApprovalPending { .. }
        )
    }

    /// 在块内打印一段文本：空串不打印，非空内容保证以换行结尾。
    fn write_block_text(text: &str) {
        if text.is_empty() {
            return;
        }
        eprint!("{text}");
        if !text.ends_with('\n') {
            eprintln!();
        }
    }
}

impl Sink for StderrSink {
    fn emit(&self, item: DisplayItem) {
        // `--quiet` 抑制过程输出，但 Error/Notice 与待批清单豁免（见 `visible_in_quiet`）。
        if self.quiet && !Self::visible_in_quiet(&item) {
            return;
        }
        match item {
            // 致命错误优先于 `--quiet`：失败原因必须对用户可见。
            DisplayItem::Error(text) => eprintln!("{text}"),
            // 运维警告（配置缺失、工作目录回退、钩子 stderr 等）同样不受 `--quiet`
            // 抑制：它们是诊断信息而非过程记录。不参与块分隔，避免卷入过程记录。
            DisplayItem::Notice(text) => eprintln!("{text}"),
            // 注入与工具调用同构：默认摘要显示注释与命令，`--verbose` 追加内容全文。
            DisplayItem::Injection {
                comment,
                command,
                content,
            } => {
                self.start_block();
                Self::write_block_text(&format!("# {comment}"));
                if let Some(command) = command.filter(|text| !text.is_empty()) {
                    Self::write_block_text(&format!("% {command}"));
                }
                if self.verbose {
                    Self::write_block_text(&content);
                }
            }
            DisplayItem::Text {
                reasoning,
                content,
                final_step,
            } => {
                let reasoning = if self.verbose {
                    reasoning.filter(|text| !text.is_empty())
                } else {
                    None
                };
                // 末步正文只由 stdout 呈现，避免终端重复。
                let content = if final_step || content.is_empty() {
                    None
                } else {
                    Some(content)
                };
                if reasoning.is_none() && content.is_none() {
                    return;
                }
                self.start_block();
                if let Some(text) = reasoning {
                    Self::write_block_text("# 思考");
                    Self::write_block_text(&text);
                }
                if let Some(text) = content {
                    Self::write_block_text(&text);
                }
            }
            DisplayItem::ToolCall {
                description,
                command,
            } => {
                self.start_block();
                if let Some(description) = description.filter(|text| !text.is_empty()) {
                    Self::write_block_text(&format!("# {description}"));
                }
                Self::write_block_text(&format!("% {command}"));
            }
            DisplayItem::ToolOutput { stdout, stderr } => {
                if !self.verbose {
                    return;
                }
                Self::write_block_text(&stdout);
                Self::write_block_text(&stderr);
            }
            DisplayItem::ApprovalPending {
                call_id,
                command,
                matched_rule,
            } => {
                self.start_block();
                let title = if matched_rule.is_empty() {
                    format!("# 待批准（调用 {call_id}）")
                } else {
                    format!("# 待批准（规则 {matched_rule}，调用 {call_id}）")
                };
                Self::write_block_text(&title);
                Self::write_block_text(&format!("% {command}"));
            }
            DisplayItem::ApprovalResolved {
                call_id,
                command,
                approved,
            } => {
                self.start_block();
                Self::write_block_text(&approval_resolved_title(&call_id, approved));
                if !command.is_empty() {
                    Self::write_block_text(&format!("% {command}"));
                }
            }
            // 压缩摘要：默认只报遮蔽条数与模型；`--verbose` 追加摘要全文。
            DisplayItem::Compaction {
                covered,
                model,
                summary,
            } => {
                self.start_block();
                Self::write_block_text(&format!("# 已压缩早期历史（{covered} 条记录，模型 {model}）"));
                if self.verbose {
                    Self::write_block_text(&summary);
                }
            }
        }
    }
}

/// 把展示条目逐条序列化为 JSONL 写到 stdout（`--json`）。
///
/// 与人类模式的双流（过程 stderr / 回复 stdout）不同，`--json` 下所有条目——含
/// 诊断与最终回复——统一走 stdout，退出码语义不变，便于机器消费（D57）。
pub struct JsonSink;

impl Sink for JsonSink {
    fn emit(&self, item: DisplayItem) {
        let value = item_json(&item);
        // `serde_json` 序列化固定结构不会失败；兜底为空对象以免破坏整条流。
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        );
    }
}

/// 展示条目的 JSON 形态：带 `type` 标签，字段名即 `DisplayItem` 的字段。
fn item_json(item: &DisplayItem) -> serde_json::Value {
    use serde_json::json;
    match item {
        DisplayItem::Notice(text) => json!({ "type": "notice", "text": text }),
        DisplayItem::Error(text) => json!({ "type": "error", "text": text }),
        DisplayItem::Injection {
            comment,
            command,
            content,
        } => json!({
            "type": "injection",
            "comment": comment,
            "command": command,
            "content": content,
        }),
        DisplayItem::Text {
            reasoning,
            content,
            final_step,
        } => json!({
            "type": "text",
            "reasoning": reasoning,
            "content": content,
            "final_step": final_step,
        }),
        DisplayItem::ToolCall {
            description,
            command,
        } => json!({
            "type": "tool_call",
            "description": description,
            "command": command,
        }),
        DisplayItem::ToolOutput { stdout, stderr } => json!({
            "type": "tool_output",
            "stdout": stdout,
            "stderr": stderr,
        }),
        DisplayItem::ApprovalPending {
            call_id,
            command,
            matched_rule,
        } => json!({
            "type": "approval_pending",
            "call_id": call_id,
            "command": command,
            "matched_rule": matched_rule,
        }),
        DisplayItem::ApprovalResolved {
            call_id,
            command,
            approved,
        } => json!({
            "type": "approval_resolved",
            "call_id": call_id,
            "command": command,
            "approved": approved,
        }),
        DisplayItem::Compaction {
            covered,
            model,
            summary,
        } => json!({
            "type": "compaction",
            "covered": covered,
            "model": model,
            "summary": summary,
        }),
    }
}

/// 审批决策块的标题行：带 `call_id` 以便多 pending 时逐条区分。
fn approval_resolved_title(call_id: &str, approved: bool) -> String {
    let verb = if approved { "已批准" } else { "已拒绝" };
    format!("# {verb}（调用 {call_id}）")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> DisplayItem {
        DisplayItem::ApprovalPending {
            call_id: "call_1".to_string(),
            command: "echo hi".to_string(),
            matched_rule: "echo *".to_string(),
        }
    }

    #[test]
    fn quiet_exempts_errors_notices_and_pending_only() {
        assert!(StderrSink::visible_in_quiet(&DisplayItem::Error(
            "失败".to_string()
        )));
        assert!(StderrSink::visible_in_quiet(&DisplayItem::Notice(
            "警告".to_string()
        )));
        assert!(
            StderrSink::visible_in_quiet(&pending()),
            "待批清单是退出码 6 的唯一结论，必须可见"
        );
        assert!(!StderrSink::visible_in_quiet(
            &DisplayItem::ApprovalResolved {
                call_id: "call_1".to_string(),
                command: "echo hi".to_string(),
                approved: true,
            }
        ));
        assert!(!StderrSink::visible_in_quiet(&DisplayItem::ToolCall {
            description: None,
            command: "echo hi".to_string(),
        }));
    }

    #[test]
    fn approval_resolved_title_includes_call_id() {
        assert_eq!(
            approval_resolved_title("call_1", true),
            "# 已批准（调用 call_1）"
        );
        assert_eq!(
            approval_resolved_title("call_2", false),
            "# 已拒绝（调用 call_2）"
        );
    }
}
