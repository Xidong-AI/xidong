//! 事件到 stderr 的渲染。
//!
//! stdout/stderr 策略完全在 cli：`yaa-core` 只上报 [`Event`]。
//!
//! - 默认：把过程记录成「块」，块与块之间用单独一行 `---` 分隔（第一个块前不打印），
//!   最终助手文本只走 stdout；
//! - `--verbose`：在默认之上追加模型思考与工具命令的输出；
//! - `--quiet`：抑制全部 stderr 事件，只留 stdout 最终回复。

use std::sync::atomic::{AtomicBool, Ordering};

use yaa_core::events::{Event, EventSink};

/// 按冗长度把事件写到 stderr。
pub struct StderrSink {
    verbose: bool,
    quiet: bool,
    /// 是否已经开过第一个过程块；据此决定后续块前是否打印 `---` 分隔线。
    block_started: AtomicBool,
}

impl StderrSink {
    /// 构造一个 stderr 事件接收器。
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
    /// 提示词钩子全部命中缓存（stderr 无任何块）时不打印；`--quiet` 下 stderr 保持干净。
    pub fn finish_transcript(&self) {
        if !self.quiet && self.block_started.load(Ordering::SeqCst) {
            eprintln!("\n---");
        }
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

impl EventSink for StderrSink {
    fn emit(&self, event: Event) {
        if self.quiet {
            return;
        }
        match event {
            // 通知不参与块分隔：原样打印，避免把警告卷进过程记录。
            Event::Notice { content } => eprintln!("{content}"),
            Event::HookStarted { name, command } => {
                self.start_block();
                Self::write_block_text(&format!("# {}", hook_description(&name)));
                Self::write_block_text(&format!("% {command}"));
            }
            Event::Ai {
                content,
                reasoning,
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
            Event::ToolStarted {
                call_id: _,
                command,
                description,
            } => {
                self.start_block();
                if let Some(description) = description.filter(|text| !text.is_empty()) {
                    Self::write_block_text(&format!("# {description}"));
                }
                Self::write_block_text(&format!("% {command}"));
            }
            Event::ToolFinished {
                call_id: _,
                code: _,
                timed_out: _,
                truncated: _,
                stdout,
                stderr,
            } => {
                if !self.verbose {
                    return;
                }
                Self::write_block_text(&stdout);
                Self::write_block_text(&stderr);
            }
        }
    }
}

/// 钩子块首行的描述映射；未知名称直接原样使用。
fn hook_description(name: &str) -> &str {
    match name {
        "system_prompt" => "注入系统提示词",
        "user_prompt" => "注入用户提示词",
        "llm" => "加载模型配置",
        other => other,
    }
}
