//! agent loop 向 CLI 上报的事件。
//!
//! `yaa-core` 只产生事件，stdout/stderr 的输出策略完全在 `yaa-cli`。

/// 一次运行中发生的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 一般通知（警告、错误提示等）。
    Notice { content: String },
    /// 钩子开始执行。
    HookStarted { name: String, command: String },
    /// 收到一次模型回复。
    Ai {
        content: String,
        reasoning: Option<String>,
        final_step: bool,
    },
    /// 工具开始执行。
    ToolStarted {
        call_id: String,
        command: String,
        description: Option<String>,
    },
    /// 工具执行结束。
    ToolFinished {
        call_id: String,
        code: i32,
        timed_out: bool,
        truncated: bool,
        stdout: String,
        stderr: String,
    },
}

/// 事件接收器。
pub trait EventSink: Send + Sync {
    /// 上报一个事件。
    fn emit(&self, event: Event);
}

/// 丢弃所有事件的空实现，供测试与静默模式使用。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: Event) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 测试用事件收集器。
    #[derive(Default)]
    struct VecSink {
        events: Mutex<Vec<Event>>,
    }

    impl VecSink {
        fn take(&self) -> Vec<Event> {
            std::mem::take(&mut self.events.lock().unwrap())
        }
    }

    impl EventSink for VecSink {
        fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn vec_sink_records_events_in_order() {
        let sink = VecSink::default();
        sink.emit(Event::HookStarted {
            name: "llm".to_string(),
            command: "cat llm.toml".to_string(),
        });
        sink.emit(Event::Ai {
            content: "你好".to_string(),
            reasoning: Some("思考".to_string()),
            final_step: false,
        });
        assert_eq!(
            sink.take(),
            vec![
                Event::HookStarted {
                    name: "llm".to_string(),
                    command: "cat llm.toml".to_string(),
                },
                Event::Ai {
                    content: "你好".to_string(),
                    reasoning: Some("思考".to_string()),
                    final_step: false,
                },
            ]
        );
    }

    #[test]
    fn null_sink_is_a_noop() {
        let sink = NullSink;
        sink.emit(Event::Notice {
            content: "ignored".to_string(),
        });
    }
}
