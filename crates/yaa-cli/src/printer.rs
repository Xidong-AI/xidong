//! 事件到 stderr 的渲染。
//!
//! stdout/stderr 策略完全在 cli：`yaa-core` 只上报 [`Event`]。
//!
//! - 默认：通知与警告走 stderr，例行的钩子/工具事件不打印；
//! - `--verbose`：额外打印钩子、模型、工具与回合事件；
//! - `--trace`：把每个工具命令打到 stderr；
//! - `--quiet`：抑制全部事件，stdout 只留最终回复。

use yaa_core::events::{Event, EventSink};

/// 按冗长度把事件写到 stderr。
pub struct StderrSink {
    verbose: bool,
    trace: bool,
    quiet: bool,
}

impl StderrSink {
    /// 构造一个 stderr 事件接收器。
    pub fn new(verbose: bool, trace: bool, quiet: bool) -> Self {
        Self {
            verbose,
            trace,
            quiet,
        }
    }
}

impl EventSink for StderrSink {
    fn emit(&self, event: Event) {
        if self.quiet {
            return;
        }
        match event {
            Event::Notice { content } => eprintln!("{content}"),
            Event::HookStarted { name, command } => {
                if self.verbose {
                    eprintln!("[钩子] {name} >>> {command}");
                }
            }
            Event::HookFinished { name, code } => {
                if self.verbose {
                    eprintln!("[钩子] {name} 退出码 {code}");
                }
            }
            Event::Ai {
                content_len,
                tool_calls,
            } => {
                if self.verbose {
                    eprintln!("[模型] 文本 {content_len} 字符，工具调用 {tool_calls} 个");
                }
            }
            Event::ToolStarted { call_id, command } => {
                if self.trace {
                    eprintln!("[工具] {command}");
                } else if self.verbose {
                    eprintln!("[工具] {call_id} 开始");
                }
            }
            Event::ToolFinished {
                call_id,
                code,
                timed_out,
                truncated,
            } => {
                if self.verbose {
                    let mut notes = String::new();
                    if timed_out {
                        notes.push_str(" 超时");
                    }
                    if truncated {
                        notes.push_str(" 截断");
                    }
                    eprintln!("[工具] {call_id} 退出码 {code}{notes}");
                }
            }
            Event::TurnEnd { reason } => {
                if self.verbose {
                    eprintln!("[回合] {reason}");
                }
            }
        }
    }
}
