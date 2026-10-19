//! 命令行参数定义。
//!
//! 阶段 1 的表面见 `docs/plan.md` 的「CLI（阶段 1）」；阶段 2 新增 `--approve`/
//! `--auto`；阶段 3 新增 `--json` 与 `--no-store`（D57/D58）；`--notify` 的 `--to`
//! 与跨会话解析后移到阶段 5。

use std::path::PathBuf;

use clap::Parser;

/// 以 Shell 为唯一工具的轻量 CLI 智能体。
#[derive(Debug, Parser)]
#[command(name = "yaa", version, about, long_about = None)]
pub struct Cli {
    /// 用户提示词；可含空格（多个词以单个空格连接），缺失时从 stdin 读到 EOF。
    ///
    /// 与 `--notify` 互斥：通知是短路写入路径，不会进入 agent loop，位置提示词
    /// 若被静默忽略会让用户误以为发起了对话（F14）。该冲突由
    /// [`Cli::validate_combinations`] 显式校验——clap 的 `conflicts_with` 会与
    /// `--to` 的 `requires = "notify"` 互相抵消，故不在此声明。
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

    /// `--notify` 的目标会话（严格校验：必须已存在且为 yaa 库；**不创建**）。
    ///
    /// 仅与 `--notify` 组合有意义：未给 `--notify` 时由 clap 报用法错误（退出码 2）。
    /// 与 `--session` 同时给出时必须指向同一数据库。
    #[arg(
        long,
        value_name = "PATH",
        requires = "notify",
        conflicts_with = "no_store"
    )]
    pub to: Option<PathBuf>,

    /// 本次运行使用临时会话，退出即淘（不读写既有会话）。
    #[arg(long, conflicts_with_all = ["session", "clear", "status", "notify", "approve"])]
    pub no_store: bool,

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
    ///
    /// 值为可选：`--notify 注释 `、`--notify=注释` 都取注释；裸 `--notify`（无值，
    /// 如 `cmd | yaa --notify`）或 `--notify=` 走缺省/空值语义。**不设**
    /// `require_equals`，否则 `--notify 注释` 会走 `default_missing_value` 而静默丢弃注释。
    ///
    /// `--notify` 是短路写入路径，不会执行 `--clear`/`--status`/`--get-prompt`，也不会
    /// 进入对话；这些组合一律按用法错误拒绝，避免静默忽略（F13/F15）。
    #[arg(
        long,
        value_name = "注释",
        num_args = 0..=1,
        default_missing_value = "由用户执行",
        conflicts_with_all = ["clear", "status", "get_prompt"]
    )]
    pub notify: Option<String>,

    /// `--notify` 要执行的命令（写在 `--` 之后）。
    #[arg(last = true, value_name = "CMD", num_args = 0..)]
    pub command: Vec<String>,

    /// 覆盖 `--notify` 命令的超时秒数（仅 `--notify` 生效）。
    #[arg(long, value_name = "SECS")]
    pub notify_timeout: Option<u64>,

    /// 放行最旧一条待批命令并继续 agent loop。
    #[arg(long, conflicts_with_all = ["notify", "clear", "status", "prompt", "get_prompt"])]
    pub approve: bool,

    /// 自动放行命中 `ask` 的命令（显式 `deny` 仍强制）。
    #[arg(long, conflicts_with_all = ["notify", "get_prompt"])]
    pub auto: bool,

    /// 以 JSONL 输出展示事件（含最终回复与诊断）；与 `--quiet`/`--verbose` 互斥。
    #[arg(long, conflicts_with_all = ["quiet", "verbose"])]
    pub json: bool,

    /// 把钩子、工具等事件打到 stderr。
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// 只在 stdout 打印最终回复。
    #[arg(long)]
    pub quiet: bool,
}

impl Cli {
    /// 解析后校验 clap 无法表达的参数组合。
    ///
    /// `--to` 的 `requires = "notify"` 与位置提示词对 `--notify` 的 `conflicts_with`
    /// 会互相抵消：给位置提示词时 clap 会放过缺 `--notify` 的 `--to`。为同时保留
    /// 「`--to` 必须搭配 `--notify`」与「`--notify` 不得忽略位置提示词」（F14），
    /// 后者在此显式校验。
    pub fn validate_combinations(&self) -> Result<(), String> {
        if self.notify.is_some() && !self.prompt.is_empty() {
            return Err(
                "`--notify` 不能与位置提示词同时使用：通知是短路写入路径，不会进入对话。"
                    .to_string(),
            );
        }
        Ok(())
    }
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
    fn notify_with_space_separated_comment_and_trailing_command() {
        // C5：`--notify 注释 -- cmd`（未用 `=`）必须把「注释」作为值，而非当成位置参数。
        let cli = Cli::parse_from(["yaa", "--notify", "注释", "--", "echo", "hi"]);
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
    fn bare_notify_falls_back_to_default_comment() {
        // 无值且无后继 token（如 `cmd | yaa --notify`）仍用缺省注释。
        let cli = Cli::parse_from(["yaa", "--notify"]);
        assert_eq!(cli.notify.as_deref(), Some("由用户执行"));
        assert!(cli.command.is_empty());
        assert!(cli.prompt.is_empty());
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

    #[test]
    fn approve_and_auto_flags_parse_and_may_combine() {
        let approve = Cli::parse_from(["yaa", "--approve"]);
        assert!(approve.approve && !approve.auto);
        let auto = Cli::parse_from(["yaa", "--auto", "你好"]);
        assert!(auto.auto && !auto.approve);
        assert_eq!(auto.prompt, vec!["你好".to_string()]);
        // `--approve` 与 `--auto` 可同时给出（先放行最旧，后续 ask 自动放行）。
        let both = Cli::parse_from(["yaa", "--approve", "--auto"]);
        assert!(both.approve && both.auto);
    }

    #[test]
    fn approve_rejects_positional_prompt() {
        // C2：`--approve` 不接受位置参数提示词；无参数时必须仍然可用。
        assert!(Cli::try_parse_from(["yaa", "--approve"]).is_ok());
        assert!(Cli::try_parse_from(["yaa", "--approve", "--auto"]).is_ok());

        let err = Cli::try_parse_from(["yaa", "--approve", "某提示"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn approve_and_auto_reject_get_prompt_combination() {
        // D4：与 `--clear`/`--status`/位置参数一致，`--get-prompt` 也不得被静默忽略。
        for args in [
            ["yaa", "--approve", "--get-prompt", "system"],
            ["yaa", "--auto", "--get-prompt", "system"],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "应为参数冲突（{args:?}）"
            );
        }
    }

    #[test]
    fn no_store_parses_alone() {
        // `--no-store` 可单独使用（可带位置提示词）。
        let cli = Cli::parse_from(["yaa", "--no-store", "你好"]);
        assert!(cli.no_store);
        assert_eq!(cli.prompt, vec!["你好".to_string()]);
    }

    #[test]
    fn json_parses_alone_and_conflicts_with_verbosity_flags() {
        let cli = Cli::parse_from(["yaa", "--json", "你好"]);
        assert!(cli.json);
        assert_eq!(cli.prompt, vec!["你好".to_string()]);

        // `--json` 已是稳定的机器接口，不再与人类可读的冗长度开关叠加。
        for args in [["yaa", "--json", "--quiet"], ["yaa", "--json", "--verbose"]] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "应为参数冲突（{args:?}）"
            );
        }
    }

    #[test]
    fn to_requires_notify() {
        // 阶段 5：`--to` 未与 `--notify` 组合时是用法错误（clap 缺必需参数，退出码 2）。
        let err = Cli::try_parse_from(["yaa", "--to", "/tmp/x.db", "你好"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn to_with_notify_parses() {
        let cli = Cli::parse_from(["yaa", "--notify=注释", "--to", "/tmp/x.db"]);
        assert_eq!(cli.notify.as_deref(), Some("注释"));
        assert_eq!(cli.to.as_deref(), Some(std::path::Path::new("/tmp/x.db")));
    }

    #[test]
    fn to_conflicts_with_no_store() {
        // `--to` 显式声明与 `--no-store` 冲突；无论是否同时给 `--notify`，都以
        // 用法错误（退出码 2）结束。
        for args in [
            vec!["yaa", "--notify=x", "--to", "/tmp/x.db", "--no-store"],
            vec!["yaa", "--to", "/tmp/x.db", "--no-store"],
        ] {
            let err = Cli::try_parse_from(&args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "应为参数冲突（{args:?}）"
            );
        }
    }

    #[test]
    fn notify_rejects_clear_status_and_get_prompt() {
        // F13/F15：`--notify` 是短路写入路径，不得静默忽略 `--clear`/`--status`/
        // `--get-prompt`；这些组合必须是用法错误（退出码 2）。
        for args in [
            vec!["yaa", "--notify=x", "--clear"],
            vec!["yaa", "--notify=x", "--status"],
            vec!["yaa", "--notify=x", "--get-prompt", "system"],
        ] {
            let err = Cli::try_parse_from(&args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "应为参数冲突（{args:?}）"
            );
            let message = err.to_string();
            assert!(
                message.contains("--notify"),
                "文案应指明 --notify（{args:?}）：{message}"
            );
        }
    }

    #[test]
    fn notify_rejects_positional_prompt() {
        // F14：`--notify` 不进入对话，位置提示词不得被静默忽略。
        // clap 的 `conflicts_with` 会与 `--to` 的 `requires` 互相抵消，故走显式校验。
        let cli = Cli::parse_from(["yaa", "--notify=注释", "提示词"]);
        let message = cli.validate_combinations().unwrap_err();
        assert!(
            message.contains("--notify"),
            "文案应指明 --notify：{message}"
        );
        assert!(
            message.contains("位置提示词"),
            "文案应指明位置提示词：{message}"
        );

        // 不带位置提示词时校验通过。
        let cli = Cli::parse_from(["yaa", "--notify=注释"]);
        assert!(cli.validate_combinations().is_ok());
    }
}
