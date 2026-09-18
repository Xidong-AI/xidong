//! `yaa` 命令行入口：解析参数、分发子命令、控制 stdout/stderr 策略。

mod cli;
mod printer;

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Parser;
use tokio::io::AsyncReadExt;
use yaa_core::agent::ExitCode;
use yaa_msg::Record;

use cli::Cli;
use printer::StderrSink;

/// 带退出码的分发错误。
///
/// `dispatch` 内部可能产生三类错误，必须映射到不同退出码：用法问题（2）、
/// 配置或钩子问题（3）、其余非预期错误（1）。退出码取自
/// [`ExitCode::code`]，避免在各处硬编码数字。
#[derive(Debug)]
struct CliError {
    exit: ExitCode,
    source: anyhow::Error,
}

impl CliError {
    /// 用法错误（退出码 2）。
    fn usage(source: impl Into<anyhow::Error>) -> Self {
        Self {
            exit: ExitCode::Usage,
            source: source.into(),
        }
    }

    /// 配置或钩子错误（退出码 3）。
    fn config(source: impl Into<anyhow::Error>) -> Self {
        Self {
            exit: ExitCode::Config,
            source: source.into(),
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(source: anyhow::Error) -> Self {
        Self {
            exit: ExitCode::Error,
            source,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(source: std::io::Error) -> Self {
        Self {
            exit: ExitCode::Error,
            source: source.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.source)
    }
}

impl std::error::Error for CliError {}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // `--get-prompt` 必须在加载配置、解析会话与执行任何钩子之前短路，否则默认
    // `system_prompt` 钩子内部调用 `yaa --get-prompt system` 会形成递归环。
    if let Some(name) = cli.get_prompt.as_deref() {
        match yaa_core::prompt::builtin(name) {
            Some(text) => {
                print!("{text}");
                std::process::exit(0);
            }
            None => {
                eprintln!("错误：未知的内置提示词 `{name}`（可用：system、user）。");
                std::process::exit(2);
            }
        }
    }

    let code = match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err}");
            err.exit.code()
        }
    };
    std::process::exit(code);
}

/// 执行一次命令，返回进程退出码。
async fn dispatch(cli: Cli) -> Result<i32, CliError> {
    // `--cwd` 改变进程工作目录，供会话 id 计算、钩子与 Shell 三处一致使用。
    // 路径不存在属用法问题（退出码 2），不是配置错误。
    if let Some(cwd) = &cli.cwd {
        std::env::set_current_dir(cwd).map_err(|err| {
            CliError::usage(anyhow!("无法切换到工作目录 {}：{err}", cwd.display()))
        })?;
    }

    let env = |key: &str| std::env::var(key).ok();
    let home = home_dir();
    let config_root = yaa_core::paths::config_root(&env, &home);
    let data_root = yaa_core::paths::data_root(&env, &home);

    if cli.clear {
        return clear(&cli, &data_root).map_err(CliError::from);
    }
    if cli.status {
        return status(&cli, &data_root).map_err(CliError::from);
    }

    run_agent(cli, &env, &home, &config_root, &data_root).await
}

/// 归档当前会话，打印目标路径。
fn clear(cli: &Cli, data_root: &Path) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let resolved = yaa_core::session::resolve(&cwd, cli.session.as_deref(), data_root)?;
    if !resolved.session.file.exists() {
        eprintln!(
            "提示：会话文件不存在，无需归档：{}",
            resolved.session.file.display()
        );
        return Ok(0);
    }

    let target = yaa_store::archive(&resolved.session, &data_root.join("history"), now_ms())?;
    println!("{}", target.display());
    Ok(0)
}

/// 打印会话摘要。
fn status(cli: &Cli, data_root: &Path) -> anyhow::Result<i32> {
    // 本函数仅做只读运维：会话解析不得创建目录（由 yaa_store::append 负责写入时创建）。

    let cwd = std::env::current_dir()?;
    let resolved = yaa_core::session::resolve(&cwd, cli.session.as_deref(), data_root)?;
    if !resolved.session.file.exists() {
        eprintln!("提示：会话文件不存在：{}", resolved.session.file.display());
        return Ok(0);
    }

    let records = yaa_store::load(&resolved.session)?;

    let mut counts = Counts::default();
    let mut last_ts = 0i64;
    let mut last_ai: Option<&str> = None;
    for record in &records {
        counts.add(record);
        last_ts = last_ts.max(record_ts(record));
        if let Record::Ai(ai) = record {
            last_ai = Some(&ai.content);
        }
    }

    println!("会话文件：{}", resolved.session.file.display());
    println!(
        "记录：{} 条（user {} / ai {} / tool_call {} / tool_result {} / system {}）",
        records.len(),
        counts.user,
        counts.ai,
        counts.tool_call,
        counts.tool_result,
        counts.system,
    );
    if let Some(text) = last_ai {
        println!("最近 ai：{}", snippet(text, 80));
    }
    println!("最近时间戳：{last_ts}");
    Ok(0)
}

/// 运行一个 agent 回合：解析配置与会话、读取提示词、驱动 `Runner`。
async fn run_agent(
    cli: Cli,
    env: &impl Fn(&str) -> Option<String>,
    home: &Path,
    config_root: &Path,
    data_root: &Path,
) -> Result<i32, CliError> {
    let sink = StderrSink::new(cli.verbose, cli.trace, cli.quiet);

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| yaa_core::paths::config_file(env, home));
    if !config_path.exists() {
        use yaa_core::events::{Event, EventSink};
        sink.emit(Event::Notice {
            content: format!("警告：配置不存在，使用默认配置：{}", config_path.display()),
        });
    }
    // 配置读取/解析失败是配置错误（退出码 3），不是通用错误。
    let config = yaa_core::config::Config::load(&config_path).map_err(CliError::config)?;

    let cwd = std::env::current_dir()?;
    let resolved = yaa_core::session::resolve(&cwd, cli.session.as_deref(), data_root)?;

    let prompt = if cli.prompt.is_empty() {
        read_stdin().await?
    } else {
        cli.prompt.join(" ")
    };

    let runner = yaa_core::agent::Runner {
        config: &config,
        config_root,
        data_root,
        cwd: &resolved.cwd_absolute,
        session: resolved.session.clone(),
        cwd_warned: resolved.warned,
        sink: &sink,
    };
    let exit = runner
        .run(&prompt, |cfg| yaa_core::agent::OpenAiModel { cfg })
        .await?;

    // stdout 只承载「成功的最终助手文本」：失败回合（模型/配置/用法错误）的原因已由
    // stderr 报告，若再打印上一条历史回复会把过期答案伪装成本次结果。
    match exit {
        ExitCode::Success => print_final_ai(&resolved.session)?,
        ExitCode::MaxSteps => {
            eprintln!(
                "警告：达到最大步数 {}，已停止；以下为当前最终回复。",
                config.agent.max_steps
            );
            print_final_ai(&resolved.session)?;
        }
        // 其他退出码：stdout 保持干净。
        _ => {}
    }

    Ok(exit.code())
}

/// 打印历史中最后一条非空 `ai` 内容；没有则什么都不打印。
fn print_final_ai(session: &yaa_store::Session) -> anyhow::Result<()> {
    let records = yaa_store::load(session)?;
    if let Some(content) = records.iter().rev().find_map(|record| match record {
        Record::Ai(ai) if !ai.content.is_empty() => Some(ai.content.as_str()),
        _ => None,
    }) {
        println!("{content}");
    }
    Ok(())
}

/// 读取 stdin 到 EOF。
async fn read_stdin() -> anyhow::Result<String> {
    let mut buffer = String::new();
    tokio::io::stdin().read_to_string(&mut buffer).await?;
    Ok(buffer)
}

/// 用户主目录，仅作为未设置 `YAA_*`/`XDG_*` 时的路径回退。
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// 当前 Unix 毫秒时间戳。
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 各类型记录计数。
#[derive(Debug, Default)]
struct Counts {
    user: usize,
    ai: usize,
    tool_call: usize,
    tool_result: usize,
    system: usize,
}

impl Counts {
    fn add(&mut self, record: &Record) {
        match record {
            Record::User(_) => self.user += 1,
            Record::Ai(_) => self.ai += 1,
            Record::ToolCall(_) => self.tool_call += 1,
            Record::ToolResult(_) => self.tool_result += 1,
            Record::System(_) => self.system += 1,
        }
    }
}

/// 记录的 Unix 毫秒时间戳。
fn record_ts(record: &Record) -> i64 {
    match record {
        Record::User(record) => record.ts,
        Record::Ai(record) => record.ts,
        Record::ToolCall(record) => record.ts,
        Record::ToolResult(record) => record.ts,
        Record::System(record) => record.ts,
    }
}

/// 把多行文本压成一行并截断到 `max_chars` 字符。
fn snippet(text: &str, max_chars: usize) -> String {
    let flat = text.replace('\n', " ");
    let mut out: String = flat.chars().take(max_chars).collect();
    if flat.chars().count() > max_chars {
        out.push('…');
    }
    out
}
