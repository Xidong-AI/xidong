//! 西东智能体核心库。
//!
//! 由原 yaa workspace 的五个 crate 模块化合并而来：
//! `msg`（记录类型）、`store`（SQLite 会话存储）、`exec`（Shell 执行）、
//! `core`（agent loop、配置、渲染、工具与模型调用）、`cli`（命令行表面）。

pub mod cli;
pub mod core;
pub mod exec;
pub mod msg;
pub mod store;
