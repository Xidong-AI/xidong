//! 命令行参数定义。
//!
//! 阶段 1 的表面见 `docs/plan.md` 的「CLI（阶段 1）」；`--json`、`--no-store`、
//! `--approve`、`--notify` 的 `--to` 等后移到阶段 2 及以后。

use std::path::PathBuf;

use clap::Parser;

/// 以 Shell 为唯一工具的轻量 CLI 智能体。
#[derive(Debug, Parser)]
#[command(name = "yaa", version, about, long_about = None)]
pub struct Cli {
    /// 用户提示词；可含空格（多个词以单个空格连接），缺失时从 stdin 读到 EOF。
    #[arg(value_name = "PROMPT", num_args = 0..)]
    pub prompt: Vec<String>,

    /// 归档当前会话。
    #[arg(long)]
    pub clear: bool,

    /// 打印会话摘要。
    #[arg(long)]
    pub status: bool,

    /// 指定会话文件。
    #[arg(long, value_name = "PATH")]
    pub session: Option<PathBuf>,

    /// 打印内置提示词（`system`）后退出。
    #[arg(long, value_name = "NAME")]
    pub get_prompt: Option<String>,

    /// 覆盖 `config.toml` 路径。
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// 改变进程工作目录（会话 id、钩子与 Shell 共用）。
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<PathBuf>,

    /// 把命令的输出作为通知写入会话；缺注释时用「由用户执行」。
    #[arg(long, value_name = "注释", num_args = 0..=1, require_equals = true, default_missing_value = "由用户执行")]
    pub notify: Option<String>,

    /// `--notify` 要执行的命令（写在 `--` 之后）。
    #[arg(last = true, value_name = "CMD", num_args = 0..)]
    pub command: Vec<String>,

    /// 把钩子、工具等事件打到 stderr。
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// 只在 stdout 打印最终回复。
    #[arg(long)]
    pub quiet: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_with_equals_and_trailing_command() {
        let cli = Cli::parse_from(["yaa", "--notify=注释", "--", "echo", "hi"]);
        assert_eq!(cli.notify.as_deref(), Some("注释"));
        assert_eq!(cli.command, vec!["echo".to_string(), "hi".to_string()]);
        assert!(cli.prompt.is_empty());
    }

    #[test]
    fn notify_without_value_defaults_comment() {
        let cli = Cli::parse_from(["yaa", "--notify", "--", "echo", "hi"]);
        assert_eq!(cli.notify.as_deref(), Some("由用户执行"));
        assert_eq!(cli.command, vec!["echo".to_string(), "hi".to_string()]);
    }

    #[test]
    fn notify_without_command_reads_stdin_shape() {
        let cli = Cli::parse_from(["yaa", "--notify=注释"]);
        assert_eq!(cli.notify.as_deref(), Some("注释"));
        assert!(cli.command.is_empty());
        assert!(cli.prompt.is_empty());
    }

    #[test]
    fn empty_notify_value_is_preserved_for_runtime_fallback() {
        let cli = Cli::parse_from(["yaa", "--notify="]);
        assert_eq!(cli.notify.as_deref(), Some(""));
    }
}
