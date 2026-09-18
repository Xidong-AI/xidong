//! 命令行参数定义。
//!
//! 阶段 1 的表面见 `docs/plan.md` 的「CLI（阶段 1）」；`--json`、`--no-store`、
//! `--approve`、`--notify` 等后移到阶段 2 及以后。

use std::path::PathBuf;

use clap::Parser;

/// 以 Shell 为唯一工具的轻量 CLI 智能体。
#[derive(Debug, Parser)]
#[command(name = "yaa", version, about, long_about = None)]
pub struct Cli {
    /// 用户提示词；缺失时从 stdin 读到 EOF。
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// 归档当前会话。
    #[arg(long)]
    pub clear: bool,

    /// 打印会话摘要。
    #[arg(long)]
    pub status: bool,

    /// 指定会话文件。
    #[arg(long, value_name = "PATH")]
    pub session: Option<PathBuf>,

    /// 打印内置提示词（`system` 或 `user`）后退出。
    #[arg(long, value_name = "NAME")]
    pub get_prompt: Option<String>,

    /// 覆盖 `config.toml` 路径。
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// 改变进程工作目录（会话 id、钩子与 Shell 共用）。
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<PathBuf>,

    /// 把钩子、工具等事件打到 stderr。
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// 只在 stdout 打印最终回复。
    #[arg(long)]
    pub quiet: bool,

    /// 把每个工具命令打到 stderr。
    #[arg(long)]
    pub trace: bool,
}
