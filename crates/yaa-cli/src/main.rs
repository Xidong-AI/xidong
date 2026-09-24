//! `yaa` 命令行入口：解析参数、分发子命令、控制 stdout/stderr 策略。

mod cli;
mod printer;

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Parser;
use tokio::io::AsyncReadExt;
use yaa_core::agent::ExitCode;
use yaa_core::display::{DisplayItem, Sink};
use yaa_msg::Record;

use cli::Cli;
use printer::{JsonSink, StderrSink};

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
    // `init` 钩子内部调用 `yaa --get-prompt system` 会形成递归环。
    if let Some(name) = cli.get_prompt.as_deref() {
        match yaa_core::prompt::builtin(name) {
            Some(text) => {
                print!("{text}");
                std::process::exit(0);
            }
            None => {
                eprintln!("错误：未知的内置提示词 `{name}`（可用：system）。");
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
    // `--no-store`：本次运行把会话指向临时目录，退出即淘。`temp_dir` 必须存活到
    // `dispatch` 返回——`main` 末尾的 `process::exit` 会跳过析构，清理只能靠这里的
    // 作用域 drop；正常返回时整个临时目录（含 `outputs/`）随 `TempDir` 一并删除。
    let temp_dir = cli.no_store.then(tempfile::TempDir::new).transpose()?;
    let data_root = temp_dir
        .as_ref()
        .map(|dir| dir.path().to_path_buf())
        .unwrap_or_else(|| yaa_core::paths::data_root(&env, &home));

    // `--notify` 是短路写入路径：不进入 agent loop、不触发 init；仅按需读取 `[shell]`
    // 限额（读取失败只告警，绝不让 notify 失败）。
    if cli.notify.is_some() {
        return notify(&cli, &env, &home, &data_root)
            .await
            .map_err(CliError::from);
    }

    if cli.clear {
        return clear(&cli, &data_root).map_err(CliError::from);
    }
    if cli.status {
        return status(&cli, &data_root).map_err(CliError::from);
    }

    run_agent(cli, &env, &home, &config_root, &data_root).await
}

/// 把命令输出或 stdin 作为通知原子写入目标会话。
///
/// 目标解析顺序：`--session` → 环境变量 `YAA_SESSION`（非空才算） → 当前 cwd
/// 默认会话（见 `docs/plan.md` D25 与规则 8）。
///
/// `--` 形态复用 `yaa-exec`：截断超长输出、完整输出落盘、退出码沿用、stderr 透传，
/// 并把 `shell_quote` 转义后的命令记入 `command`（可重放）。
async fn notify(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    home: &Path,
    data_root: &Path,
) -> anyhow::Result<i32> {
    let session = notify_session(cli, env, data_root)?;
    // 缺注释或注释为空串时统一用「由用户执行」。
    let comment = match cli.notify.as_deref() {
        Some(text) if !text.is_empty() => text,
        _ => "由用户执行",
    };

    let limits = notify_limits(cli, env, home);

    if cli.command.is_empty() {
        // stdin 形态：复用 `yaa-exec` 的截断/落盘策略，与 `--` 形态完全一致：
        // 超限时正文为头尾拼接并带标记，完整输入落盘并记入 `output_files`。
        let outputs_dir = yaa_core::session::outputs_dir(&session);
        let captured = yaa_exec::capture_reader(std::io::stdin(), &limits, &outputs_dir).await?;
        let output_files = captured.file.as_deref().map(|path| yaa_msg::OutputFiles {
            stdout: Some(yaa_core::tools::shell::relativize(path, data_root)),
            stderr: None,
        });
        let content = envelope(comment, &captured.text);
        yaa_store::append(
            &session,
            &[Record::system("notify", content, None, output_files)],
        )?;
        return Ok(0);
    }

    // `--` 形态：与 `shell` 工具同一条执行路径。
    let command = shell_quote(&cli.command);
    let outputs_dir = yaa_core::session::outputs_dir(&session);
    let output = yaa_exec::run(
        &yaa_exec::ShellCommand {
            call_id: String::new(),
            command: command.clone(),
            env: Vec::new(),
        },
        &limits,
        &outputs_dir,
    )
    .await?;

    // stderr 原样透传到 yaa 的 stderr，保持「透传」语义。
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    // 被信号杀死（`code < 0`）按通用错误码 1；真正超时额外提示。
    let exit = if output.code < 0 { 1 } else { output.code };
    if output.timed_out {
        eprintln!("--notify 命令超时，已终止。");
    }

    let output_files = notify_output_files(&output, data_root);
    let content = envelope(comment, &output.stdout);
    yaa_store::append(
        &session,
        &[Record::system(
            "notify",
            content,
            Some(command),
            output_files,
        )],
    )?;
    Ok(exit)
}

/// 把正文包进 `<注释>…</注释>` 信封，去掉结尾换行（design.md 的示例形态）。
///
/// 注释中的换行（`\r`/`\n`）先压成空格：`display::envelope_comment` 要求首行即
/// `<注释>`，多行注释会让信封解析失败、展示标题退化为「通知」。
fn envelope(comment: &str, body: &str) -> String {
    let comment: String = comment
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    format!("<{comment}>\n{}\n</{comment}>", body.trim_end_matches('\n'))
}

/// 解析 `--notify` 的限额，优先级从高到低：`--notify-timeout`（仅超时）→ 环境变量
/// `YAA_NOTIFY_*`（能解析为数字才生效）→ 配置文件 `[shell]` → 内置默认。
///
/// 配置读取或解析失败时只打警告到 stderr 并退回默认值——`--notify` 不得因此失败。
fn notify_limits(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    home: &Path,
) -> yaa_exec::ShellLimits {
    let mut shell = yaa_core::config::ShellConfig::default();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| yaa_core::paths::config_file(env, home));
    match yaa_core::config::Config::load(&config_path) {
        Ok(config) => shell = config.shell,
        Err(err) => eprintln!("警告：读取配置失败，--notify 使用默认限额：{err:#}"),
    }

    if let Some(value) = parse_env(env, "YAA_NOTIFY_TIMEOUT_SECS") {
        shell.timeout_secs = value;
    }
    if let Some(value) = parse_env(env, "YAA_NOTIFY_MAX_STREAM_CHARS") {
        shell.max_stream_chars = value;
    }
    if let Some(value) = parse_env(env, "YAA_NOTIFY_HEAD_CHARS") {
        shell.head_chars = value;
    }
    if let Some(value) = parse_env(env, "YAA_NOTIFY_TAIL_CHARS") {
        shell.tail_chars = value;
    }
    if let Some(value) = cli.notify_timeout {
        shell.timeout_secs = value;
    }

    shell.to_limits()
}

/// 读取环境变量并解析为 `T`；缺失或无法解析都返回 `None`（静默不生效）。
fn parse_env<T: std::str::FromStr>(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<T> {
    env(key).and_then(|value| value.trim().parse::<T>().ok())
}

/// 由 `yaa-exec` 的完整输出路径构造 `output_files`，相对数据根；无文件则 `None`。
fn notify_output_files(
    output: &yaa_exec::ShellOutput,
    data_root: &Path,
) -> Option<yaa_msg::OutputFiles> {
    if output.stdout_file.is_none() && output.stderr_file.is_none() {
        return None;
    }
    let stdout = output
        .stdout_file
        .as_deref()
        .map(|path| yaa_core::tools::shell::relativize(path, data_root));
    let stderr = output
        .stderr_file
        .as_deref()
        .map(|path| yaa_core::tools::shell::relativize(path, data_root));
    Some(yaa_msg::OutputFiles { stdout, stderr })
}

/// 把 argv 逐个 POSIX 单引号转义后以空格连接成一条命令字符串。
///
/// 简单 token（仅含安全字符）不加引号；含空格、单引号或其他特殊字符时用单引号包裹，
/// 内部单引号按 `'\''` 转义。结果可交给 `sh -c` 重放。
fn shell_quote(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_token(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 单个 token 的转义。
fn quote_token(token: &str) -> String {
    if is_simple_token(token) {
        token.to_string()
    } else {
        format!("'{}'", token.replace('\'', "'\\''"))
    }
}

/// token 是否只含无需转义的安全字符。
fn is_simple_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,^".contains(c))
}

/// 解析 `--notify` 的目标会话；显式路径按当前 cwd 补全为绝对路径。
fn notify_session(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> anyhow::Result<yaa_store::Session> {
    Ok(resolve_session_target(cli, env, data_root)?.session)
}

/// 解析会话目标：`--session` → `YAA_SESSION`（非空才算）→ 当前 cwd 默认会话。
///
/// 供 `--notify` 与 `--approve` 共用，保证两者的目标解析链完全一致（规则 8）。
fn resolve_session_target(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> anyhow::Result<yaa_core::session::Resolved> {
    let cwd = std::env::current_dir()?;
    let env_session = env("YAA_SESSION");
    yaa_core::session::resolve_target(
        &cwd,
        cli.session.as_deref(),
        env_session.as_deref(),
        data_root,
    )
}

/// 一次 agent 回合的会话解析：`--approve` 走扩展链（同 `--notify`）；普通 run 保持
/// 阶段 1 语义（仅 `--session`，忽略 `YAA_SESSION`，嵌套会话语义留待阶段 5）。
fn resolve_run_session(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> anyhow::Result<yaa_core::session::Resolved> {
    if cli.approve {
        return resolve_session_target(cli, env, data_root);
    }
    let cwd = std::env::current_dir()?;
    yaa_core::session::resolve(&cwd, cli.session.as_deref(), data_root)
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
        "记录：{} 条（user {} / ai {} / tool_call {} / tool_result {} / system {} / approval {}）",
        records.len(),
        counts.user,
        counts.ai,
        counts.tool_call,
        counts.tool_result,
        counts.system,
        counts.approval,
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
    // `--json` 把所有展示条目序列化到 stdout（D57）；其余情况走 stderr 的人类可读渲染。
    let stderr_sink = StderrSink::new(cli.verbose, cli.quiet);
    let json_sink = JsonSink;
    let sink: &dyn Sink = if cli.json { &json_sink } else { &stderr_sink };

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| yaa_core::paths::config_file(env, home));
    if !config_path.exists() {
        sink.emit(DisplayItem::Notice(format!(
            "警告：配置不存在，使用默认配置：{}",
            config_path.display()
        )));
    }
    // 配置读取/解析失败是配置错误（退出码 3），不是通用错误。
    let config = yaa_core::config::Config::load(&config_path).map_err(CliError::config)?;

    let resolved = resolve_run_session(&cli, env, data_root)?;

    let prompt = if cli.approve {
        // `--approve` 不读提示词、不写 stdin 提示。
        String::new()
    } else if cli.prompt.is_empty() {
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
        auto: cli.auto,
        sink,
    };
    // Ctrl-C：向当前工具进程组转发 SIGINT，宽限内仍存活则 SIGKILL 兜底，再以 130 退出。
    // 无工具在运行时（模型请求中）直接退出；`--notify` 短路路径不启动该任务。
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            yaa_exec::interrupt_current_group(std::time::Duration::from_secs(2)).await;
            std::process::exit(130);
        }
    });
    // 运行前的水位：只允许打印本次运行新产生、且非空的最末 `ai` 正文，避免在空回复
    // 或不完整回合里把上一条历史回复误当成本次结果。
    let watermark = yaa_store::load(&resolved.session)
        .map_err(anyhow::Error::from)?
        .len();
    let request_timeout = std::time::Duration::from_secs(config.agent.request_timeout_secs);
    // 客户端构造一次、经 clone 共享连接池（`Client` 内部为 Arc，克隆廉价）。
    let client = yaa_core::llm::build_client(request_timeout)?;
    let exit = if cli.approve {
        let client = client.clone();
        runner
            .approve_pending(move |cfg| yaa_core::agent::OpenAiModel { cfg, client })
            .await?
    } else {
        let client = client.clone();
        runner
            .run(&prompt, move |cfg| yaa_core::agent::OpenAiModel {
                cfg,
                client,
            })
            .await?
    };

    // stdout 只承载「成功的最终助手文本」：失败回合（模型/配置/用法错误）的原因已由
    // stderr 报告，若再打印上一条历史回复会把过期答案伪装成本次结果。
    // `--json` 下最终回复已作为 `text` 事件进入 JSONL，不再单独打印。
    match exit {
        ExitCode::Success => {
            if !cli.json {
                stderr_sink.finish_transcript();
                print_final_ai(&resolved.session, watermark)?;
            }
        }
        ExitCode::MaxSteps => {
            sink.emit(DisplayItem::Notice(format!(
                "警告：达到最大步数 {}，已停止；以下为当前最终回复。",
                config.agent.max_steps
            )));
            if !cli.json {
                stderr_sink.finish_transcript();
                print_final_ai(&resolved.session, watermark)?;
            }
        }
        // 其他退出码：stdout 保持干净。
        _ => {}
    }

    Ok(exit.code())
}

/// 打印 `records[watermark..]` 中**最后一条** `ai` 记录的内容；为空则不打印。
///
/// 只看最后一条 `ai`（而非最后一条非空 `ai`），否则末步为空回复时会把本轮中间
/// 步骤的正文当成本次结果打到 stdout（A5）。
fn print_final_ai(session: &yaa_store::Session, watermark: usize) -> anyhow::Result<()> {
    let records = yaa_store::load(session)?;
    let start = watermark.min(records.len());
    if let Some(Record::Ai(ai)) = records[start..]
        .iter()
        .rev()
        .find(|record| matches!(record, Record::Ai(_)))
        && !ai.content.is_empty()
    {
        println!("{}", ai.content);
    }
    Ok(())
}

/// 读取 stdin 到 EOF。
///
/// 先读为字节再 lossy 解码：非 UTF-8 输入（如二进制管道）仍能作为提示词继续，
/// 非法字节以替换字符呈现，而不是让整个回合以 IO 错误结束、丢弃已读内容。
async fn read_stdin() -> anyhow::Result<String> {
    let mut buffer = Vec::new();
    tokio::io::stdin().read_to_end(&mut buffer).await?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
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
    /// `approval_pending` 与 `approval_resolved` 合并口径，使分类之和等于总数。
    approval: usize,
}

impl Counts {
    fn add(&mut self, record: &Record) {
        match record {
            Record::User(_) => self.user += 1,
            Record::Ai(_) => self.ai += 1,
            Record::ToolCall(_) => self.tool_call += 1,
            Record::ToolResult(_) => self.tool_result += 1,
            Record::System(_) => self.system += 1,
            Record::ApprovalPending(_) | Record::ApprovalResolved(_) => self.approval += 1,
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
        Record::ApprovalPending(record) => record.ts,
        Record::ApprovalResolved(record) => record.ts,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_simple_tokens_unquoted() {
        assert_eq!(shell_quote(&["echo".into(), "hi".into()]), "echo hi");
        assert_eq!(
            shell_quote(&["seq".into(), "1".into(), "200".into()]),
            "seq 1 200"
        );
    }

    #[test]
    fn shell_quote_wraps_and_escapes_special_tokens() {
        let quoted = shell_quote(&["echo".into(), "a b".into(), "it's".into()]);
        assert_eq!(quoted, "echo 'a b' 'it'\\''s'");
    }

    #[test]
    fn shell_quote_round_trips_through_sh() {
        // 重放等价性：转义后交给 `sh -c` 应还原原 argv。
        let args = vec![
            "printf".to_string(),
            "%s|".to_string(),
            "a b'c \"d\" $e".to_string(),
        ];
        let command = shell_quote(&args);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{command} z"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "a b'c \"d\" $e|z|",
            "转义后的命令应能重放原 argv"
        );
    }

    #[test]
    fn envelope_flattens_comment_newlines() {
        // C7：注释含换行时信封首行仍须是完整 `<注释>`，供 `display::envelope_comment` 解析。
        assert_eq!(envelope("a\nb", "hi\n"), "<a b>\nhi\n</a b>");
        assert_eq!(envelope("a\r\nb", "hi"), "<a  b>\nhi\n</a  b>");
    }

    #[test]
    fn parse_env_only_accepts_numbers() {
        let env = |key: &str| match key {
            "YAA_NOTIFY_TIMEOUT_SECS" => Some(" 7 ".to_string()),
            "YAA_NOTIFY_HEAD_CHARS" => Some("abc".to_string()),
            _ => None,
        };
        assert_eq!(parse_env::<u64>(&env, "YAA_NOTIFY_TIMEOUT_SECS"), Some(7));
        assert_eq!(parse_env::<usize>(&env, "YAA_NOTIFY_HEAD_CHARS"), None);
        assert_eq!(parse_env::<u64>(&env, "MISSING"), None);
    }

    #[test]
    fn run_session_uses_env_only_for_approve() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        let env_path = dir.path().join("from-env.jsonl");
        let env =
            |key: &str| (key == "YAA_SESSION").then(|| env_path.to_string_lossy().into_owned());

        // C1：`--approve` 认 YAA_SESSION，与 `--notify` 解析链一致。
        let approve = Cli::parse_from(["yaa", "--approve"]);
        let resolved = resolve_run_session(&approve, &env, &data_root).unwrap();
        assert_eq!(
            resolved.session.file,
            env_path,
            "--approve 应认 YAA_SESSION：{}",
            resolved.session.file.display()
        );

        // `--session` 优先于 YAA_SESSION。
        let explicit = dir.path().join("explicit.jsonl");
        let approve =
            Cli::parse_from(["yaa", "--approve", "--session", explicit.to_str().unwrap()]);
        let resolved = resolve_run_session(&approve, &env, &data_root).unwrap();
        assert_eq!(resolved.session.file, explicit, "--session 应最优先");

        // 普通 run 的会话解析保持不变：忽略 YAA_SESSION，仍走 cwd 默认。
        let normal = Cli::parse_from(["yaa", "你好"]);
        let resolved = resolve_run_session(&normal, &env, &data_root).unwrap();
        assert!(
            resolved.session.root.starts_with(&data_root),
            "普通 run 不得因 YAA_SESSION 改变目标：{}",
            resolved.session.file.display()
        );
    }

    #[test]
    fn status_counts_include_approval_and_sum_to_total() {
        let records = vec![
            Record::user("你好"),
            Record::ai("model", "hash", "答", None),
            Record::tool_call(yaa_msg::ToolOrigin::Ai, "c1", "echo hi", None),
            Record::tool_result("c1", "hi\n", "", 0, false, false, None),
            Record::system("notify", "x", None, None),
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", yaa_msg::ApprovalDecision::Approve),
        ];
        let mut counts = Counts::default();
        for record in &records {
            counts.add(record);
        }
        assert_eq!(counts.approval, 2, "待批与决策都应计入审批项");
        assert_eq!(
            counts.user
                + counts.ai
                + counts.tool_call
                + counts.tool_result
                + counts.system
                + counts.approval,
            records.len(),
            "分类计数之和必须等于记录总数",
        );
    }
}
