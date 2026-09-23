//! 从记录投影出的展示条目。
//!
//! 展示与模型消息同源（`docs/plan.md` D15）：两者都从同一份事件日志派生，
//! `items` 是纯函数，不维护第二套执行事件流。`yaa-cli` 只决定样式与 stdout/stderr
//! 分流（D18）。

use yaa_msg::{Record, ToolOrigin};

/// 一条展示条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayItem {
    /// 进程级提示（非记录投影），`--quiet` 下可被抑制。
    Notice(String),
    /// 致命错误（模型调用失败、钩子执行/解析失败等）；`--quiet` 下仍须可见。
    Error(String),
    /// 注入内容：初始化通知或外部通知。
    Injection {
        /// 信封注释；非信封时为「通知」。
        comment: String,
        /// 产生该通知的命令，仅用于展示。
        command: Option<String>,
        /// 注入正文（含信封）。
        content: String,
    },
    /// 模型正文；`final_step` 为真表示其后无紧邻工具调用。
    Text {
        reasoning: Option<String>,
        content: String,
        final_step: bool,
    },
    /// 模型发起的工具调用。
    ToolCall {
        description: Option<String>,
        command: String,
    },
    /// 工具输出。
    ToolOutput { stdout: String, stderr: String },
}

/// 展示条目的接收端。
pub trait Sink: Send + Sync {
    /// 上报一条展示条目。
    fn emit(&self, item: DisplayItem);
}

/// 丢弃全部条目的接收端。
pub struct NullSink;

impl Sink for NullSink {
    fn emit(&self, _item: DisplayItem) {}
}

/// 把一段记录投影为展示条目（纯函数）。
pub fn items(records: &[Record]) -> Vec<DisplayItem> {
    let mut items = Vec::new();
    for (index, record) in records.iter().enumerate() {
        match record {
            Record::System(system) => {
                if system.source == "notify" && !system.content.is_empty() {
                    items.push(DisplayItem::Injection {
                        comment: envelope_comment(&system.content)
                            .unwrap_or("通知")
                            .to_string(),
                        command: system.command.clone(),
                        content: system.content.clone(),
                    });
                }
                // `init` 簿记与空 `system` 不产出条目。
            }
            Record::Ai(ai) => {
                let final_step = !matches!(
                    records.get(index + 1),
                    Some(Record::ToolCall(call)) if call.origin == ToolOrigin::Ai
                );
                items.push(DisplayItem::Text {
                    reasoning: ai.reasoning.clone(),
                    content: ai.content.clone(),
                    final_step,
                });
            }
            Record::ToolCall(call) if call.origin == ToolOrigin::Ai => {
                items.push(DisplayItem::ToolCall {
                    description: call.description.clone(),
                    command: call.command.clone(),
                });
            }
            Record::ToolResult(result) => items.push(DisplayItem::ToolOutput {
                stdout: result.stdout.clone(),
                stderr: result.stderr.clone(),
            }),
            // `tool_call(origin=user)` 不产出条目。
            _ => {}
        }
    }
    items
}

/// 从 `<注释>\n…\n</注释>` 信封提取注释；非信封返回 `None`。
///
/// 首行必须是 `<注释>`，末行必须是 `</注释>`，两者注释一致才认作信封。
pub fn envelope_comment(content: &str) -> Option<&str> {
    let newline = content.find('\n')?;
    let header = &content[..newline];
    let comment = header.strip_prefix('<')?.strip_suffix('>')?;
    let closing = format!("\n</{comment}>");
    if content.ends_with(&closing) {
        Some(comment)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(content: &str, command: Option<&str>) -> Record {
        Record::system("notify", content, command.map(str::to_string))
    }

    #[test]
    fn init_and_empty_systems_produce_nothing() {
        let records = vec![
            Record::system("init", "", None),
            Record::system("notify", "", None),
        ];
        assert!(items(&records).is_empty());
    }

    #[test]
    fn notify_projects_injection_with_envelope_comment() {
        let records = vec![notify(
            "<系统提示词>\n内容\n</系统提示词>",
            Some("yaa --get-prompt system"),
        )];
        let items = items(&records);
        assert_eq!(
            items,
            vec![DisplayItem::Injection {
                comment: "系统提示词".to_string(),
                command: Some("yaa --get-prompt system".to_string()),
                content: "<系统提示词>\n内容\n</系统提示词>".to_string(),
            }]
        );
    }

    #[test]
    fn non_envelope_notify_defaults_comment_to_notice() {
        let records = vec![notify("裸文本", None)];
        let items = items(&records);
        assert_eq!(
            items,
            vec![DisplayItem::Injection {
                comment: "通知".to_string(),
                command: None,
                content: "裸文本".to_string(),
            }]
        );
    }

    #[test]
    fn ai_text_final_step_depends_on_adjacent_ai_call() {
        let records = vec![
            Record::ai("m", "h", "中途", Some("想".into())),
            Record::tool_call(ToolOrigin::Ai, "c1", "ls", Some("列目录".into())),
            Record::ai("m", "h", "最终", None),
        ];
        let items = items(&records);
        assert_eq!(
            items,
            vec![
                DisplayItem::Text {
                    reasoning: Some("想".to_string()),
                    content: "中途".to_string(),
                    final_step: false,
                },
                DisplayItem::ToolCall {
                    description: Some("列目录".to_string()),
                    command: "ls".to_string(),
                },
                DisplayItem::Text {
                    reasoning: None,
                    content: "最终".to_string(),
                    final_step: true,
                },
            ]
        );
    }

    #[test]
    fn empty_ai_still_projects_text() {
        let items = items(&[Record::ai("m", "h", "", None)]);
        assert_eq!(
            items,
            vec![DisplayItem::Text {
                reasoning: None,
                content: String::new(),
                final_step: true,
            }]
        );
    }

    #[test]
    fn tool_result_projects_output() {
        let records = vec![Record::tool_result(
            "c1", "out", "err", 0, false, false, None,
        )];
        assert_eq!(
            items(&records),
            vec![DisplayItem::ToolOutput {
                stdout: "out".to_string(),
                stderr: "err".to_string(),
            }]
        );
    }

    #[test]
    fn user_origin_call_produces_nothing() {
        let records = vec![Record::tool_call(ToolOrigin::User, "u1", "ls", None)];
        assert!(items(&records).is_empty());
    }

    #[test]
    fn envelope_comment_rejects_non_envelopes() {
        assert_eq!(envelope_comment("<注释>\n正文\n</注释>"), Some("注释"));
        assert_eq!(envelope_comment("<注释>\n</注释>"), Some("注释"));
        assert_eq!(envelope_comment("没有信封"), None);
        assert_eq!(envelope_comment("<注释>\n正文"), None);
        assert_eq!(envelope_comment("<注释>\n正文\n</不匹配>"), None);
    }
}
