//! 展示条目到 stderr 的渲染。
//!
//! stdout/stderr 策略完全在 cli：`yaa-core` 只投影 [`DisplayItem`]（D15/D18）。
//!
//! - 默认：把过程记录成「块」，块与块之间用单独一行 `---` 分隔（第一个块前不打印），
//!   最终助手文本只走 stdout；
//! - `--verbose`：在默认之上追加模型思考、命令输出与注入内容全文；
//! - `--quiet`：抑制全部过程输出，但**致命错误仍打印到 stderr**（否则用户看不到失败原因）。

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
        match item {
            // 致命错误优先于 `--quiet`：失败原因必须对用户可见。
            DisplayItem::Error(text) => eprintln!("{text}"),
            // `--quiet` 抑制其余全部过程输出。
            _ if self.quiet => {}
            // 通知不参与块分隔：原样打印，避免把警告卷进过程记录。
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
        }
    }
}
