//! 工具定义。
//!
//! 阶段 1 只有 `shell` 一个工具；它的全部知识（function schema、提示词 section、
//! 参数解析、错误文案、调用编排与结果转换）内聚于 [`shell`]（见 `docs/plan.md` D14）。

pub mod shell;
