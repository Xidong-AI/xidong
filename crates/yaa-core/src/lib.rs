//! `yaa-core`：配置、agent loop、渲染、钩子与模型调用。
//!
//! 阶段 1 先交付配置、路径、钩子、提示词、消息与模型适配等基础模块；
//! agent loop、渲染与 CLI 由后续提交填充。

pub mod config;
pub mod hook;
pub mod llm;
pub mod message;
pub mod paths;
pub mod prompt;

/// 骨架占位，保证 crate 有可编译的公开项。
pub fn placeholder() -> &'static str {
    "yaa-core 尚未实现"
}
