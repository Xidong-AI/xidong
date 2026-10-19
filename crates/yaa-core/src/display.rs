//! 从记录投影出的展示条目。
//!
//! 展示与模型消息同源（`docs/plan.md` D15）：两者都从同一份事件日志派生，
//! `items` 是纯函数，不维护第二套执行事件流。`yaa-cli` 只决定样式与 stdout/stderr
//! 分流（D18）。

use std::collections::BTreeMap;

use yaa_msg::{OutputFiles, Record, System, ToolOrigin};

/// 一条展示条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayItem {
    /// 诊断提示（非记录投影：配置缺失、cwd 回退、钩子 stderr 等）；`--quiet` 下仍可见。
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
    /// 待批工具调用（阶段 2）：命令与命中的规则。
    ApprovalPending {
        call_id: String,
        command: String,
        matched_rule: String,
    },
    /// 审批决策（阶段 2）：`approved` 为真表示已批准；`command` 为被决策调用的命令。
    ApprovalResolved {
        call_id: String,
        command: String,
        approved: bool,
    },
    /// 压缩摘要（阶段 3）：早期历史被折叠为一条摘要；展示仍保留原始记录。
    ///
    /// 默认只展示遮蔽条数与模型，`--verbose` 追加摘要全文。
    Compaction {
        /// 被遮蔽的记录条数。
        covered: usize,
        /// 生成摘要的模型名。
        model: String,
        /// 摘要正文。
        summary: String,
    },
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
    items_from(records, 0)
}

/// 从 `start` 起投影展示条目，但 `records` 仍须传**全量**记录。
///
/// 增量展示只把「水位之后」的记录切片交给本函数（见 `agent::Runner::emit_new`），
/// 而产生命令的 `tool_call` 记录在更早批次里；若命令表只从展示切片构建，审批决策块
/// 的 `command` 将恒为空。故把「命令表来源（全量）」与「展示起点」分离。
pub fn items_from(records: &[Record], start: usize) -> Vec<DisplayItem> {
    // `call_id` → 命令：审批决策块需回带被决策的命令（R5）；同 id 以后者为准。
    let commands: BTreeMap<&str, &str> = records
        .iter()
        .filter_map(|record| match record {
            Record::ToolCall(call) => Some((call.call_id.as_str(), call.command.as_str())),
            _ => None,
        })
        .collect();

    let start = start.min(records.len());
    let mut items = Vec::new();
    for (offset, record) in records[start..].iter().enumerate() {
        // `index` 为全量记录中的绝对下标，供 `final_step` 判断下一条记录。
        let index = start + offset;
        match record {
            Record::System(system) => {
                if system.source == "notify" && has_visible_text(system) {
                    items.push(DisplayItem::Injection {
                        comment: envelope_comment(&system.content)
                            .unwrap_or("通知")
                            .to_string(),
                        command: system.command.clone(),
                        content: system_text(system),
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
            // 审批记录不参与模型消息投影，但要在 stderr 展示待批与决策（阶段 2）。
            Record::ApprovalPending(pending) => items.push(DisplayItem::ApprovalPending {
                call_id: pending.call_id.clone(),
                command: pending.command.clone(),
                matched_rule: pending.matched_rule.clone(),
            }),
            Record::ApprovalResolved(resolved) => items.push(DisplayItem::ApprovalResolved {
                call_id: resolved.call_id.clone(),
                command: commands
                    .get(resolved.call_id.as_str())
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
                approved: resolved.decision == yaa_msg::ApprovalDecision::Approve,
            }),
            // `tool_call(origin=user)` 不产出条目。
            _ => {}
        }
    }
    items
}

/// `final_step` 待定的 `ai`：真正判定取决于其后**紧跟**的记录，须等下一批到达。
struct PendingText {
    reasoning: Option<String>,
    content: String,
}

/// 增量展示投影器：把**逐批**到达的记录投影为展示条目，行为等价于全量 [`items_from`]。
///
/// [`items_from`] 需要全量记录：它预扫命令表（供审批决策回填 `command`），并按
/// 「下一条记录」判定 `ai` 的 `final_step`。跟随者只有增量，不能每批全量重投影，故本
/// 结构跨批维护这两项状态：
///
/// - **命令表**：每条 `ToolCall` 到达时更新（`call_id → command`）；
/// - **待定正文**：到达的 `ai` 不立即产出 [`DisplayItem::Text`]，而是挂起，待下一条记录
///   到达时按「是否为 `origin=ai` 的 `ToolCall`」判定 `final_step` 再产出。流结束时用
///   [`DisplayProjector::flush`] 产出末尾挂起的正文（此时其后无记录，`final_step=true`）。
///
/// 等价性：对任意历史 `H` 与切分点 `k`，把 `H` 分两批推送（先 `H[..k]` 后 `H[k..]`）
/// 并最终 `flush()`，其条目拼接 == `items_from(H, 0)`。
/// 前提是 `H` 中引用 `call_id` 的审批记录出现在产生该 `call_id` 的 `ToolCall` 之后
/// （真实历史始终如此）。
#[derive(Default)]
pub struct DisplayProjector {
    commands: BTreeMap<String, String>,
    pending: Option<PendingText>,
}

impl DisplayProjector {
    /// 空投影器，用于从某一水位开始跟随。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用已有历史初始化状态（接管时回填命令表）。
    ///
    /// 传入的历史须**已展示完毕**：本方法只累积命令表，不产出任何条目，并清空末尾待定
    /// 正文，避免接管后重复展示前缀最后一条 `ai`。
    pub fn from_records(records: &[Record]) -> Self {
        let mut projector = Self::new();
        let mut discard = Vec::new();
        for record in records {
            projector.feed(record, &mut discard);
            discard.clear();
        }
        projector.pending = None;
        projector
    }

    /// 推入一批增量记录，返回本批应展示的条目（不含末尾待定的 `ai` 正文）。
    pub fn push(&mut self, rows: &[Record]) -> Vec<DisplayItem> {
        let mut items = Vec::new();
        for record in rows {
            self.feed(record, &mut items);
        }
        items
    }

    /// 产出末尾挂起的 `ai` 正文（其后无记录，按轮末处理）。流结束时调用一次。
    pub fn flush(&mut self) -> Vec<DisplayItem> {
        match self.pending.take() {
            Some(pending) => vec![DisplayItem::Text {
                reasoning: pending.reasoning,
                content: pending.content,
                final_step: true,
            }],
            None => Vec::new(),
        }
    }

    /// 处理单条记录：先按本记录判定并产出上一条挂起的 `ai`，再产出本记录的条目。
    fn feed(&mut self, record: &Record, items: &mut Vec<DisplayItem>) {
        if let Some(pending) = self.pending.take() {
            let final_step = !matches!(
                record,
                Record::ToolCall(call) if call.origin == ToolOrigin::Ai
            );
            items.push(DisplayItem::Text {
                reasoning: pending.reasoning,
                content: pending.content,
                final_step,
            });
        }
        match record {
            Record::ToolCall(call) => {
                self.commands
                    .insert(call.call_id.clone(), call.command.clone());
            }
            Record::Ai(ai) => {
                self.pending = Some(PendingText {
                    reasoning: ai.reasoning.clone(),
                    content: ai.content.clone(),
                });
            }
            _ => {}
        }
        match record {
            Record::System(system) => {
                if system.source == "notify" && has_visible_text(system) {
                    items.push(DisplayItem::Injection {
                        comment: envelope_comment(&system.content)
                            .unwrap_or("通知")
                            .to_string(),
                        command: system.command.clone(),
                        content: system_text(system),
                    });
                }
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
            Record::ApprovalPending(pending) => items.push(DisplayItem::ApprovalPending {
                call_id: pending.call_id.clone(),
                command: pending.command.clone(),
                matched_rule: pending.matched_rule.clone(),
            }),
            Record::ApprovalResolved(resolved) => items.push(DisplayItem::ApprovalResolved {
                call_id: resolved.call_id.clone(),
                command: self
                    .commands
                    .get(resolved.call_id.as_str())
                    .cloned()
                    .unwrap_or_default(),
                approved: resolved.decision == yaa_msg::ApprovalDecision::Approve,
            }),
            // `ai`（已挂起）与 `tool_call(origin=user)` 不产出条目。
            _ => {}
        }
    }
}

/// `own_id` 之后**第一条轮末 `ai`** 的正文；找不到则 `None`。
///
/// 「轮末」指其后（全量历史中）紧跟的不是 `origin=ai` 的 `ToolCall`。用于把最终回复
/// 归位到自己的 user 记录：多客户端合并一轮时，各自打印自己 id 之后的那条答案。
pub fn final_ai_for_own<'a>(records: &'a [Record], own_id: &str) -> Option<&'a str> {
    let start = records.iter().position(|record| record.id() == own_id)?;
    for (offset, record) in records[start + 1..].iter().enumerate() {
        if let Record::Ai(ai) = record {
            let index = start + 1 + offset;
            let final_step = !matches!(
                records.get(index + 1),
                Some(Record::ToolCall(call)) if call.origin == ToolOrigin::Ai
            );
            if final_step {
                return Some(&ai.content);
            }
        }
    }
    None
}

/// 历史是否「空闲」（无未完成回合）：空、末条为轮末 `ai`、或仅含 init 簿记/空 `system`。
///
/// 末条为 `user` / `tool_result` / `tool_call` / 审批记录都视为未完成（需继续跑）；含正文的
/// `notify` 也已消费却尚未得到模型回应，同样视为未完成。
pub fn is_idle(records: &[Record]) -> bool {
    match records.last() {
        None => true,
        Some(Record::Ai(_)) => true,
        Some(Record::System(_)) => records.iter().all(|record| {
            matches!(
                record,
                Record::System(system) if system.source == "init" || system.content.is_empty()
            )
        }),
        _ => false,
    }
}

/// 统一截断提示：正文被截断时说明仅保留头尾，并给出完整输出相对路径。
///
/// [`crate::render`] 的工具消息与 [`system_text`] 都调用本函数，避免两处文案漂移。
/// `files` 两个流都缺路径时省略路径（超出上限却未落盘的情形）。
pub fn truncation_note(files: &OutputFiles) -> String {
    let mut note = String::from("[truncated] 仅保留头尾输出，完整内容见输出文件");
    if let Some(path) = files.stdout.as_deref().or(files.stderr.as_deref()) {
        note.push(' ');
        note.push_str(path);
    }
    note.push('。');
    note
}

/// `system` 投影进上下文的正文。
///
/// 即 `system.content`；若带 `output_files`（正文被截断），追加 [`truncation_note`]。
/// 注意这里**不**拼 `[执行命令]` 前缀——命令前缀由 [`crate::render`] 在投影时按需
/// 加在最前，展示层用独立的 `% 命令` 呈现。
pub fn system_text(system: &System) -> String {
    let mut text = system.content.clone();
    if let Some(files) = &system.output_files {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&truncation_note(files));
    }
    text
}

/// `system` 是否有可展示、可投影的正文：非空 `content` 或落盘了完整输出。
fn has_visible_text(system: &System) -> bool {
    !system.content.is_empty() || system.output_files.is_some()
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
        Record::system("notify", content, command.map(str::to_string), None)
    }

    fn notify_truncated(command: Option<&str>) -> Record {
        Record::system(
            "notify",
            "<注释>\n头尾\n</注释>",
            command.map(str::to_string),
            Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/01J.stdout.out".to_string()),
                stderr: None,
            }),
        )
    }

    #[test]
    fn init_and_empty_systems_produce_nothing() {
        let records = vec![
            Record::system("init", "", None, None),
            Record::system("notify", "", None, None),
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
    fn truncation_note_includes_relative_path_or_omits_it() {
        let with_stdout = OutputFiles {
            stdout: Some("sessions/sid/outputs/a.stdout.out".to_string()),
            stderr: None,
        };
        assert_eq!(
            truncation_note(&with_stdout),
            "[truncated] 仅保留头尾输出，完整内容见输出文件 sessions/sid/outputs/a.stdout.out。"
        );

        let only_stderr = OutputFiles {
            stdout: None,
            stderr: Some("sessions/sid/outputs/a.stderr.out".to_string()),
        };
        assert!(truncation_note(&only_stderr).contains("a.stderr.out"));

        let no_path = OutputFiles {
            stdout: None,
            stderr: None,
        };
        assert_eq!(
            truncation_note(&no_path),
            "[truncated] 仅保留头尾输出，完整内容见输出文件。"
        );
    }

    #[test]
    fn system_text_appends_truncation_note_only_when_output_files_present() {
        let plain = System {
            id: "s".into(),
            ts: 1,
            source: "notify".into(),
            command: None,
            output_files: None,
            content: "正文".into(),
        };
        assert_eq!(system_text(&plain), "正文");

        let truncated = System {
            output_files: Some(OutputFiles {
                stdout: Some("sessions/sid/outputs/a.stdout.out".into()),
                stderr: None,
            }),
            ..plain
        };
        assert_eq!(
            system_text(&truncated),
            "正文\n[truncated] 仅保留头尾输出，完整内容见输出文件 sessions/sid/outputs/a.stdout.out。"
        );
    }

    #[test]
    fn truncated_notify_projects_injection_with_note_and_command_field() {
        let items = items(&[notify_truncated(Some("seq 1 9"))]);
        assert_eq!(
            items,
            vec![DisplayItem::Injection {
                comment: "注释".to_string(),
                command: Some("seq 1 9".to_string()),
                content:
                    "<注释>\n头尾\n</注释>\n[truncated] 仅保留头尾输出，完整内容见输出文件 sessions/sid/outputs/01J.stdout.out。"
                        .to_string(),
            }]
        );
    }

    #[test]
    fn approval_records_project_display_items() {
        let records = vec![
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Deny),
        ];
        assert_eq!(
            items(&records),
            vec![
                DisplayItem::ApprovalPending {
                    call_id: "c1".to_string(),
                    command: "echo hi".to_string(),
                    matched_rule: "echo *".to_string(),
                },
                // 无对应 `tool_call` 记录时命令为空串。
                DisplayItem::ApprovalResolved {
                    call_id: "c1".to_string(),
                    command: String::new(),
                    approved: false,
                },
            ]
        );
    }

    /// `tool_call` 与 `resolved` 同在传入切片内时命令可回填（纯函数属性）。
    #[test]
    fn approval_resolved_carries_command_when_tool_call_in_slice() {
        let records = vec![
            Record::tool_call(ToolOrigin::Ai, "c1", "echo hi", Some("打招呼".into())),
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
        ];
        let resolved = items(&records)
            .into_iter()
            .find_map(|item| match item {
                DisplayItem::ApprovalResolved {
                    call_id,
                    command,
                    approved,
                } => Some((call_id, command, approved)),
                _ => None,
            })
            .expect("应有审批决策条目");
        assert_eq!(resolved, ("c1".to_string(), "echo hi".to_string(), true));
    }

    /// R5b 真实形态：增量展示只传入水位之后的切片，`tool_call` 在更早批次里；
    /// 命令表须基于全量记录构建，否则「已批准/已拒绝」块恒无命令。
    #[test]
    fn approval_resolved_fills_command_from_earlier_batch() {
        let records = vec![
            Record::tool_call(ToolOrigin::Ai, "c1", "echo hi", None),
            Record::ai("m", "h", "", None),
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
        ];
        // 展示起点取 2：切片内只有 pending 与 resolved，没有 tool_call。
        let items = items_from(&records, 2);
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, DisplayItem::ToolCall { .. })),
            "展示切片内不应含 tool_call：{items:?}"
        );
        let resolved = items
            .into_iter()
            .find_map(|item| match item {
                DisplayItem::ApprovalResolved {
                    call_id,
                    command,
                    approved,
                } => Some((call_id, command, approved)),
                _ => None,
            })
            .expect("应有审批决策条目");
        assert_eq!(
            resolved,
            ("c1".to_string(), "echo hi".to_string(), true),
            "命令应来自更早批次的 tool_call"
        );
    }

    #[test]
    fn envelope_comment_rejects_non_envelopes() {
        assert_eq!(envelope_comment("<注释>\n正文\n</注释>"), Some("注释"));
        assert_eq!(envelope_comment("<注释>\n</注释>"), Some("注释"));
        assert_eq!(envelope_comment("没有信封"), None);
        assert_eq!(envelope_comment("<注释>\n正文"), None);
        assert_eq!(envelope_comment("<注释>\n正文\n</不匹配>"), None);
    }

    /// 构造一份覆盖各记录类型的代表性历史。
    fn sample_history() -> Vec<Record> {
        vec![
            notify("<系统>\n提示\n</系统>", Some("yaa --get-prompt system")),
            Record::system("init", "", None, None),
            Record::user("你好"),
            Record::ai("m", "h", "中途", Some("想".into())),
            Record::tool_call(ToolOrigin::Ai, "c1", "echo hi", Some("打招呼".into())),
            Record::tool_result("c1", "hi\n", "", 0, false, false, None),
            Record::ai("m", "h", "最终", None),
        ]
    }

    /// 任意切分点下，分两批推送（含末尾 flush）拼接 == 全量 `items_from`。
    #[test]
    fn projector_matches_full_projection_for_every_split() {
        let history = sample_history();
        let full = items_from(&history, 0);
        for split in 0..=history.len() {
            let mut projector = DisplayProjector::new();
            let mut got = projector.push(&history[..split]);
            got.extend(projector.push(&history[split..]));
            got.extend(projector.flush());
            assert_eq!(got, full, "切分点 {split} 的增量投影应与全量一致");
        }
    }

    /// 多批推送（含末尾 flush）与全量一致；`final_step` 跨批正确解析。
    #[test]
    fn projector_matches_full_projection_across_many_pushes() {
        let history = sample_history();
        let full = items_from(&history, 0);
        let mut projector = DisplayProjector::new();
        let mut got = Vec::new();
        for record in &history {
            got.extend(projector.push(std::slice::from_ref(record)));
        }
        got.extend(projector.flush());
        assert_eq!(got, full, "逐条推送应与全量一致");
    }

    /// `ai` 与紧随的 `tool_call` 分处两批时，`final_step` 仍应为 false。
    #[test]
    fn projector_resolves_final_step_across_batches() {
        let ai = Record::ai("m", "h", "中途", None);
        let call = Record::tool_call(ToolOrigin::Ai, "c1", "ls", None);
        let mut projector = DisplayProjector::new();
        assert!(
            projector.push(std::slice::from_ref(&ai)).is_empty(),
            "末尾的 ai 应挂起，待下一条判定 final_step"
        );
        let items = projector.push(std::slice::from_ref(&call));
        assert_eq!(
            items,
            vec![
                DisplayItem::Text {
                    reasoning: None,
                    content: "中途".to_string(),
                    final_step: false,
                },
                DisplayItem::ToolCall {
                    description: None,
                    command: "ls".to_string(),
                },
            ]
        );
        assert!(projector.flush().is_empty(), "已无挂起正文");
    }

    /// 审批决策的 `command` 由更早批次的 `tool_call` 回填（跨批命令表）。
    #[test]
    fn projector_backfills_approval_command_from_earlier_batch() {
        let mut projector = DisplayProjector::new();
        projector.push(&[Record::tool_call(ToolOrigin::Ai, "c1", "echo hi", None)]);
        let items = projector.push(&[
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
        ]);
        assert_eq!(
            items,
            vec![
                DisplayItem::ApprovalPending {
                    call_id: "c1".to_string(),
                    command: "echo hi".to_string(),
                    matched_rule: "echo *".to_string(),
                },
                DisplayItem::ApprovalResolved {
                    call_id: "c1".to_string(),
                    command: "echo hi".to_string(),
                    approved: true,
                },
            ]
        );
    }

    /// `from_records` 初始化（前缀视作已展示）后可继续跨批投影且命令表齐全。
    #[test]
    fn projector_from_records_carries_state_into_push() {
        let history = sample_history();
        let mut projector = DisplayProjector::from_records(&history);
        let extra = vec![Record::approval_resolved(
            "c1",
            yaa_msg::ApprovalDecision::Deny,
        )];
        let items = projector.push(&extra);
        assert_eq!(
            items,
            vec![DisplayItem::ApprovalResolved {
                call_id: "c1".to_string(),
                command: "echo hi".to_string(),
                approved: false,
            }]
        );
    }

    #[test]
    fn final_ai_for_own_returns_first_final_step_after_own() {
        let own = Record::user("问");
        let own_id = own.id().to_string();
        let history = vec![
            own,
            Record::ai("m", "h", "中途", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "ls", None),
            Record::tool_result("c1", "out", "", 0, false, false, None),
            Record::ai("m", "h", "答案", None),
        ];
        assert_eq!(final_ai_for_own(&history, &own_id), Some("答案"));
    }

    #[test]
    fn final_ai_for_own_is_none_when_own_missing_or_no_ai() {
        let history = vec![Record::user("问"), Record::ai("m", "h", "答", None)];
        assert_eq!(final_ai_for_own(&history, "不存在"), None);
        assert_eq!(
            final_ai_for_own(&[history[0].clone()], history[0].id()),
            None
        );
    }

    #[test]
    fn is_idle_covers_empty_ai_last_and_system_only() {
        assert!(is_idle(&[]));
        assert!(is_idle(&[Record::system("init", "", None, None)]));
        assert!(is_idle(&[
            Record::user("问"),
            Record::ai("m", "h", "答", None),
        ]));
        assert!(
            !is_idle(&[notify("正文", None)]),
            "含正文的 notify 尚未得到回应，不应判为空闲"
        );
        assert!(!is_idle(&[Record::user("问")]));
        assert!(!is_idle(&[
            Record::user("问"),
            Record::ai("m", "h", "", None),
            Record::tool_call(ToolOrigin::Ai, "c1", "ls", None),
        ]));
        assert!(!is_idle(&[Record::approval_pending("c1", "ls", "ls *")]));
    }
}
