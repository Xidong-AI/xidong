//! `xidong` 命令行入口：解析参数、分发子命令、控制 stdout/stderr 策略。

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Parser;
use tokio::io::AsyncReadExt;
use xidong::core::agent::ExitCode;
use xidong::core::display::{DisplayItem, Sink};
use xidong::msg::Record;

use xidong::cli::Cli;
use xidong::cli::printer::{JsonSink, StderrSink};

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

    // clap 无法表达的组合（F14）：在短路/加载配置之前按用法错误退出。
    if let Err(message) = cli.validate_combinations() {
        eprintln!("错误：{message}");
        std::process::exit(2);
    }

    // `--get-prompt` 必须在加载配置、解析会话与执行任何钩子之前短路，否则默认
    // `init` 钩子内部调用 `xidong --get-prompt system` 会形成递归环。
    if let Some(name) = cli.get_prompt.as_deref() {
        match xidong::core::prompt::builtin(name) {
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
    let config_root = xidong::core::paths::config_root(&env, &home);
    // `--no-store`：本次运行把会话指向临时目录，退出即淘。`temp_dir` 必须存活到
    // `dispatch` 返回——`main` 末尾的 `process::exit` 会跳过析构，清理只能靠这里的
    // 作用域 drop；正常返回时整个临时目录（含 `outputs/`）随 `TempDir` 一并删除。
    let temp_dir = cli.no_store.then(tempfile::TempDir::new).transpose()?;
    let data_root = temp_dir
        .as_ref()
        .map(|dir| dir.path().to_path_buf())
        .unwrap_or_else(|| xidong::core::paths::data_root(&env, &home));

    // `--notify` 是短路写入路径：不进入 agent loop、不触发 init；仅按需读取 `[shell]`
    // 限额（读取失败只告警，绝不让 notify 失败）。
    if cli.notify.is_some() {
        // 目标解析先于任何目录创建：显式 `--to` 的用法/严格校验失败不得留下文件。
        let target = resolve_notify_target_for_cli(&cli, &env, &data_root)?;
        return notify(&cli, &env, &home, &data_root, target)
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
/// 目标已由 [`resolve_notify_target_for_cli`] 按 spec 第 2 节解析：显式 `--to` →
/// `--session` → `XIDONG_NOTIFY` → `XIDONG_SESSION` → cwd 默认。
///
/// `--` 形态复用 `xidong::exec`：截断超长输出、完整输出落盘、退出码沿用、stderr 透传，
/// 并把 `shell_quote` 转义后的命令记入 `command`（可重放）。
async fn notify(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    home: &Path,
    data_root: &Path,
    target: xidong::core::session::NotifyTarget,
) -> anyhow::Result<i32> {
    let session = &target.resolved.session;
    // 目标不存在时明确告警后照常创建（design.md：发现目标不存在时 stderr 警告）。
    // `XIDONG_SESSION` 来自钩子内部调用时，首次创建是预期行为，不打扰用户。
    // 显式 `--to` 已在解析阶段严格校验存在性，既不会走到这里，也绝不创建。
    if !target.explicit && !session.db.exists() && env("XIDONG_SESSION").is_none() {
        eprintln!("警告：目标会话不存在，将创建：{}", session.db.display());
    }
    // clear.lock 需要父目录存在；写入本来也会创建会话目录。显式目标必须已存在，
    // 这里跳过创建以兑现「绝不创建目标目录」。
    if !target.explicit {
        std::fs::create_dir_all(&session.root)?;
    }
    // 尽力清理崩溃残留的发布临时文件（1 小时阈值，与 run 路径一致）。
    xidong::store::cleanup_inbox_tmp(session, std::time::Duration::from_secs(3600));
    xidong::store::cleanup_spool_tmp(session, std::time::Duration::from_secs(3600));
    // 归档预检：短暂持 clear.lock 共享锁并串行 init，取出即放，绝不覆盖命令执行。
    // 这样 notify 命令内嵌 `xidong --clear` 时不会形成「子等父释放 SH、父等父结束」的死锁。
    // 同时捕获发布时用于校验的 generation：命令执行期间若发生真归档，发布会失败。
    //
    // 预检失败若属可重试类别（BUSY/IO），不阻断通知：记录后在发布阶段直接降级 spool，
    // 命令退出码保持不变（F9）。归档等终态仍然报错。
    let precheck = (|| -> Result<Option<i64>, xidong::store::StoreError> {
        let _clear_guard = xidong::store::FileLock::shared(&session.clear_lock_path())?;
        let init_guard = xidong::store::FileLock::exclusive(&session.init_lock_path())?;
        let archived = xidong::store::is_archived(session)?;
        let generation = xidong::store::generation(session)?;
        drop(init_guard);
        if archived {
            return Ok(None);
        }
        Ok(Some(generation))
    })();
    let guard = match precheck {
        Ok(Some(generation)) => NotifyGuard::Generation(generation),
        Ok(None) => {
            eprintln!("错误：会话已归档，不可写入：{}", session.db.display());
            return Ok(1);
        }
        Err(err) if err.is_retryable() => NotifyGuard::PrecheckFailed(err),
        Err(err) => return Err(err.into()),
    };
    // 缺注释或注释为空串时统一用「由用户执行」。
    let comment = match cli.notify.as_deref() {
        Some(text) if !text.is_empty() => text,
        _ => "由用户执行",
    };
    // 归因：`origin` 总是写发布者会话库绝对路径；`call_id` 仅在继承值合法时写。
    // `origin` 是可选字段：解析失败（如 cwd 存在旧 `default.jsonl`）只省略归因，
    // 绝不让合法的显式 `--to` 发布失败（F4）。
    let origin = publisher_origin(cli, env, data_root);
    let call_id = notify_call_id(env("XIDONG_NOTIFY_CALL_ID").as_deref());

    let limits = notify_limits(cli, env, home);

    if cli.command.is_empty() {
        // stdin 形态：复用 `xidong::exec` 的截断/落盘策略，与 `--` 形态完全一致：
        // 超限时正文为头尾拼接并带标记，完整输入落盘并记入 `output_files`。
        let outputs_dir = xidong::core::session::outputs_dir(session);
        let captured =
            xidong::exec::capture_reader(std::io::stdin(), &limits, &outputs_dir).await?;
        let output_files = captured
            .file
            .as_deref()
            .map(|path| xidong::msg::OutputFiles {
                stdout: Some(xidong::core::tools::shell::relativize(path, data_root)),
                stderr: None,
            });
        let content = envelope(comment, &captured.text);
        let record =
            Record::system_with_attribution("notify", content, None, output_files, origin, call_id);
        publish_notify(session, env, &record, guard, target.is_self)?;
        return Ok(0);
    }

    // `--` 形态：与 `shell` 工具同一条执行路径。
    let command = shell_quote(&cli.command);
    let outputs_dir = xidong::core::session::outputs_dir(session);
    let output = xidong::exec::run(
        &xidong::exec::ShellCommand {
            call_id: String::new(),
            command: command.clone(),
            env: Vec::new(),
        },
        &limits,
        &outputs_dir,
    )
    .await?;

    // stderr 原样透传到 xidong 的 stderr，保持「透传」语义。
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
    let record = Record::system_with_attribution(
        "notify",
        content,
        Some(command),
        output_files,
        origin,
        call_id,
    );
    publish_notify(session, env, &record, guard, target.is_self)?;
    Ok(exit)
}

/// 发布者会话库的绝对路径，作为通知的 `origin`。
///
/// 继承了 `XIDONG_SESSION` 时直接使用其值（钩子/工具调用链中的自身会话）；否则用
/// [`resolve_session_target`] 解析出的自发会话（`--session` → cwd 默认）。
///
/// 本函数是 best-effort：解析失败（如 cwd 默认会话目录存在旧 `default.jsonl`）时
/// 返回 `None`，由调用方省略可选的 `origin` 字段，绝不阻断通知发布（F4）。
fn publisher_origin(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> Option<String> {
    if let Some(path) = env("XIDONG_SESSION").filter(|value| !value.is_empty()) {
        let absolute = std::path::absolute(Path::new(&path)).ok()?;
        return Some(absolute.to_string_lossy().into_owned());
    }
    let resolved = resolve_session_target(cli, env, data_root).ok()?;
    let absolute = std::path::absolute(&resolved.session.db).ok()?;
    Some(absolute.to_string_lossy().into_owned())
}

/// 从 `XIDONG_NOTIFY_CALL_ID` 取出合法调用 id：非空且匹配 `[A-Za-z0-9_-]{1,128}`。
///
/// 非法值静默忽略（不报错、不写字段），避免把外部输入带进记录。
fn notify_call_id(raw: Option<&str>) -> Option<String> {
    let value = raw?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(value.to_owned())
}

/// 发布前的 generation 守卫状态。
enum NotifyGuard {
    /// 预检成功：发布时校验该 generation。
    Generation(i64),
    /// 预检因 BUSY/IO 失败：无法安全校验 generation，直接降级 spool（F9）。
    PrecheckFailed(xidong::store::StoreError),
}

/// 把通知发布到文件 inbox（阶段 4 刀 2）。不再直接写 events：消息由 runner 在安全点消费落库。
///
/// phase 由 `XIDONG_INBOX_PHASE == "init"` 决定（init 钩子内注入），且仅对自发发布
/// （目标 == 自身会话）生效；跨会话目标（含显式 `--to`）一律写 `pending/`。
/// 发布走 [`xidong::store::publish_with_generation`]：内部持 `clear.lock` 共享锁并与
/// `--clear` 的排他锁互斥，原子校验预检时捕获的 `expected` generation。命令执行期间
/// 发生真归档（generation 变化且非 0→1 建库）时拒绝发布，避免把通知写进新 generation。
/// 命令执行阶段不持锁（`notify` 内嵌 `xidong --clear` 不会死锁）。
fn publish_notify(
    session: &xidong::store::Session,
    env: &impl Fn(&str) -> Option<String>,
    record: &Record,
    guard: NotifyGuard,
    is_self: bool,
) -> anyhow::Result<()> {
    let phase = if is_self && env("XIDONG_INBOX_PHASE").as_deref() == Some("init") {
        xidong::store::InboxPhase::Init
    } else {
        xidong::store::InboxPhase::Pending
    };
    match guard {
        NotifyGuard::Generation(expected) => {
            match xidong::store::publish_with_generation(session, phase, record, Some(expected)) {
                Ok(()) => Ok(()),
                Err(xidong::store::StoreError::GenerationChanged { .. }) => {
                    anyhow::bail!("会话已归档，通知未写入。")
                }
                // 可重试类失败（BUSY/IO）：保底写 spool，下轮运行补投。命令退出码保持不变。
                Err(err) if err.is_retryable() => {
                    spool_notify(session, record, Some(expected), &err.to_string())
                }
                Err(err) => Err(err.into()),
            }
        }
        NotifyGuard::PrecheckFailed(err) => {
            // 预检 BUSY/IO：不阻断命令，发布阶段直接降级 spool（F9）。
            spool_notify(session, record, None, &err.to_string())
        }
    }
}

/// 把通知写入 spool；失败时区分「已归档」终态与 spool 写入错误。
///
/// `expected` 传给 [`xidong::store::publish_spool_with_generation`]，与 inbox 发布共用
/// generation/archived 守卫，避免 spool 绕过 `--clear` 的检查（F10）。
fn spool_notify(
    session: &xidong::store::Session,
    record: &Record,
    expected: Option<i64>,
    cause: &str,
) -> anyhow::Result<()> {
    match xidong::store::publish_spool_with_generation(session, record, expected) {
        Ok(_) => {
            eprintln!(
                "警告：通知发布失败（{cause}），已写入 spool，将在下次运行时补投：{}",
                session.spool_dir().display()
            );
            Ok(())
        }
        Err(xidong::store::StoreError::GenerationChanged { .. })
        | Err(xidong::store::StoreError::Archived { .. }) => {
            anyhow::bail!("会话已归档，通知未写入。")
        }
        Err(spool_err) => Err(anyhow::anyhow!(
            "通知发布失败（{cause}），写入 spool 亦失败：{spool_err}"
        )),
    }
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
/// `XIDONG_NOTIFY_*`（能解析为数字才生效）→ 配置文件 `[shell]` → 内置默认。
///
/// 配置读取或解析失败时只打警告到 stderr 并退回默认值——`--notify` 不得因此失败。
fn notify_limits(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    home: &Path,
) -> xidong::exec::ShellLimits {
    let mut shell = xidong::core::config::ShellConfig::default();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| xidong::core::paths::config_file(env, home));
    match xidong::core::config::Config::load(&config_path) {
        Ok(config) => shell = config.shell,
        Err(err) => eprintln!("警告：读取配置失败，--notify 使用默认限额：{err:#}"),
    }

    if let Some(value) = parse_env(env, "XIDONG_NOTIFY_TIMEOUT_SECS") {
        shell.timeout_secs = value;
    }
    if let Some(value) = parse_env(env, "XIDONG_NOTIFY_MAX_STREAM_CHARS") {
        shell.max_stream_chars = value;
    }
    if let Some(value) = parse_env(env, "XIDONG_NOTIFY_HEAD_CHARS") {
        shell.head_chars = value;
    }
    if let Some(value) = parse_env(env, "XIDONG_NOTIFY_TAIL_CHARS") {
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

/// 由 `xidong::exec` 的完整输出路径构造 `output_files`，相对数据根；无文件则 `None`。
fn notify_output_files(
    output: &xidong::exec::ShellOutput,
    data_root: &Path,
) -> Option<xidong::msg::OutputFiles> {
    if output.stdout_file.is_none() && output.stderr_file.is_none() {
        return None;
    }
    let stdout = output
        .stdout_file
        .as_deref()
        .map(|path| xidong::core::tools::shell::relativize(path, data_root));
    let stderr = output
        .stderr_file
        .as_deref()
        .map(|path| xidong::core::tools::shell::relativize(path, data_root));
    Some(xidong::msg::OutputFiles { stdout, stderr })
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

/// 解析 `--notify` 的目标，并把用法错误（退出码 2）与目标错误（退出码 1）分开。
fn resolve_notify_target_for_cli(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> Result<xidong::core::session::NotifyTarget, CliError> {
    use xidong::core::session::{NotifyTargetError, resolve_notify_target};

    let cwd = std::env::current_dir()?;
    resolve_notify_target(
        &cwd,
        cli.to.as_deref(),
        cli.session.as_deref(),
        data_root,
        env,
    )
    .map_err(|err| match err {
        NotifyTargetError::Usage(message) => CliError::usage(anyhow!(message)),
        NotifyTargetError::Other(err) => CliError::from(err),
    })
}

/// 解析会话目标：`--session` → `XIDONG_SESSION`（非空才算）→ 当前 cwd 默认会话。
///
/// 供 `--approve` 使用；`--notify` 走 [`resolve_notify_target_for_cli`] 的扩展链
/// （`--to`/`XIDONG_NOTIFY`），`--approve` 不认 `--to`。
fn resolve_session_target(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> anyhow::Result<xidong::core::session::Resolved> {
    let cwd = std::env::current_dir()?;
    let env_session = env("XIDONG_SESSION");
    xidong::core::session::resolve_target(
        &cwd,
        cli.session.as_deref(),
        env_session.as_deref(),
        data_root,
    )
}

/// 一次 agent 回合的会话解析：`--approve` 走扩展链（同 `--notify`）；普通 run 保持
/// 阶段 1 语义（仅 `--session`，忽略 `XIDONG_SESSION`，嵌套会话语义留待阶段 5）。
fn resolve_run_session(
    cli: &Cli,
    env: &impl Fn(&str) -> Option<String>,
    data_root: &Path,
) -> anyhow::Result<xidong::core::session::Resolved> {
    if cli.approve {
        return resolve_session_target(cli, env, data_root);
    }
    let cwd = std::env::current_dir()?;
    xidong::core::session::resolve(&cwd, cli.session.as_deref(), data_root)
}

/// 归档当前会话，打印目标路径。
fn clear(cli: &Cli, data_root: &Path) -> anyhow::Result<i32> {
    let history_root = data_root.join("history");
    // 旧 JSONL 走文件级归档；`resolve` 会拒绝旧格式，因此必须先于解析处理。
    if let Some(path) = cli.session.as_deref() {
        let absolute = std::path::absolute(path)?;
        if is_legacy_jsonl(&absolute) {
            if !absolute.exists() {
                eprintln!("提示：会话文件不存在，无需归档：{}", absolute.display());
                return Ok(0);
            }
            let target = xidong::store::archive_legacy_jsonl(&absolute, &history_root, now_ms())?;
            println!("{}", target.display());
            return Ok(0);
        }
    }

    let cwd = std::env::current_dir()?;
    let resolved = xidong::core::session::resolve(&cwd, cli.session.as_deref(), data_root)?;
    if !resolved.session.db.exists() {
        // 库不存在也要检查 inbox：仅剩待处理消息时归档会静默丢消息，必须拒绝。
        let (pending, init, spool) = pending_counts(&resolved.session)?;
        if pending + init + spool > 0 {
            report_pending(pending, init, spool);
            return Ok(1);
        }
        eprintln!(
            "提示：会话文件不存在，无需归档：{}",
            resolved.session.db.display()
        );
        return Ok(0);
    }

    let Some(_runner_lock) =
        xidong::store::FileLock::try_exclusive(&resolved.session.runner_lock_path())?
    else {
        eprintln!(
            "错误：会话正在运行，无法归档：{}",
            resolved.session.db.display()
        );
        return Ok(1);
    };
    let clear_path = resolved.session.clear_lock_path();
    let _clear_lock = match xidong::store::FileLock::try_exclusive(&clear_path)? {
        Some(lock) => lock,
        None => {
            eprintln!("会话正在写入，等待完成…");
            xidong::store::FileLock::exclusive(&clear_path)?
        }
    };
    // 持两把锁后检查待处理消息：notify 发布持 clear.lock SH，clear 取 EX，
    // 因此检查期间不会有新发布插入；有消息则拒绝归档并保留 inbox。
    let (pending, init, spool) = pending_counts(&resolved.session)?;
    if pending + init + spool > 0 {
        report_pending(pending, init, spool);
        return Ok(1);
    }
    let target = xidong::store::archive(&resolved.session, &history_root, now_ms())?;
    println!("{}", target.display());
    Ok(0)
}

/// 统计 `pending/`、`init/` 与 spool 中的待处理消息数（只读：目录不存在视为 0）。
fn pending_counts(session: &xidong::store::Session) -> anyhow::Result<(usize, usize, usize)> {
    // 只读计数：不解析、不移动文件。malformed/超限文件同样阻止归档（与 --status 一致），
    // 且 clear 在拒绝路径上不得产生 rejected/ 等副作用。
    let pending = xidong::store::count_inbox(session, xidong::store::InboxPhase::Pending)?;
    let init = xidong::store::count_inbox(session, xidong::store::InboxPhase::Init)?;
    let spool = xidong::store::count_spool(session)?;
    Ok((pending, init, spool))
}

/// 报告待处理消息并提示处理方式（调用方负责返回退出码 1）。
fn report_pending(pending: usize, init: usize, spool: usize) {
    eprintln!(
        "错误：会话有待处理消息（{} 条，pending {pending} / init {init} / spool {spool}），请先运行 xidong 处理或手动移除。",
        pending + init + spool
    );
}

/// 打印待处理消息计数（`--status` 摘要的一行；格式保持阶段 4 不变）。
fn print_pending_inbox(pending: usize, init: usize) {
    println!(
        "待处理消息：{} 条（pending {pending} / init {init}）",
        pending + init
    );
}

/// 打印 spool 滞留计数（`--status` 摘要的一行）。
fn print_spool_count(spool: usize) {
    println!("滞留 spool：{spool} 条");
}

/// 路径是否以 `.jsonl` 结尾（旧版会话格式）。
fn is_legacy_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

/// 打印会话摘要。
fn status(cli: &Cli, data_root: &Path) -> anyhow::Result<i32> {
    // 本函数仅做只读运维：会话解析不得创建目录（由 xidong::store::append 负责写入时创建）。

    let cwd = std::env::current_dir()?;
    let resolved = xidong::core::session::resolve(&cwd, cli.session.as_deref(), data_root)?;
    if !resolved.session.db.exists() {
        // 只读计数：不解析、不 reject、不创建目录（B8）。
        let (pending, init, spool) = pending_counts(&resolved.session)?;
        eprintln!("提示：会话文件不存在：{}", resolved.session.db.display());
        // 库缺失但 inbox/spool 有消息时仍显示计数，避免待处理消息完全不可见。
        if pending + init > 0 {
            print_pending_inbox(pending, init);
        }
        print_spool_count(spool);
        return Ok(0);
    }

    let records = xidong::store::load(&resolved.session)?;

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

    println!("会话文件：{}", resolved.session.db.display());
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
    let (pending, init, spool) = pending_counts(&resolved.session)?;
    print_pending_inbox(pending, init);
    print_spool_count(spool);
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
        .unwrap_or_else(|| xidong::core::paths::config_file(env, home));
    if !config_path.exists() {
        sink.emit(DisplayItem::Notice(format!(
            "警告：配置不存在，使用默认配置：{}",
            config_path.display()
        )));
    }
    // 配置读取/解析失败是配置错误（退出码 3），不是通用错误。
    let config = xidong::core::config::Config::load(&config_path).map_err(CliError::config)?;

    let resolved = resolve_run_session(&cli, env, data_root)?;

    // 会话目录必须先存在：发布 inbox、取锁都需要它。
    std::fs::create_dir_all(&resolved.session.root)?;

    // 提示词需在发布 own 之前读定（发布即写入消息内容）；`--approve` 不读提示词。
    let prompt = if cli.approve {
        String::new()
    } else if cli.prompt.is_empty() {
        read_stdin().await?
    } else {
        cli.prompt.join(" ")
    };

    let runner = xidong::core::agent::Runner {
        config: &config,
        config_root,
        data_root,
        cwd: &resolved.cwd_absolute,
        session: resolved.session.clone(),
        cwd_warned: resolved.warned,
        auto: cli.auto,
        sink,
    };

    let request_timeout = std::time::Duration::from_secs(config.agent.request_timeout_secs);
    // 客户端构造一次、经 clone 共享连接池（`Client` 内部为 Arc，克隆廉价）。
    let client = xidong::core::llm::build_client(request_timeout)?;

    // `--approve`：不发布 user、不跟随；抢锁失败仍报错退出 1（保持阶段 2 语义）。
    if cli.approve {
        let Some(_runner_lock) =
            xidong::store::FileLock::try_exclusive(&resolved.session.runner_lock_path())
                .map_err(anyhow::Error::from)?
        else {
            eprintln!(
                "错误：会话正在运行，另一次 xidong 已持有该会话：{}",
                resolved.session.db.display()
            );
            return Ok(ExitCode::Error.code());
        };
        spawn_interrupt_handler();
        // 运行前的水位：`--approve` 无 own，保持水位语义，只打印本次新产生的最末 `ai`。
        let watermark = xidong::store::last_seq(&resolved.session).map_err(anyhow::Error::from)?;
        let client = client.clone();
        let exit = runner
            .approve_pending(move |cfg| xidong::core::agent::OpenAiModel { cfg, client })
            .await?;
        print_outcome(exit, &cli, &stderr_sink, sink, &config, || {
            print_final_ai(&resolved.session, watermark)
        })?;
        return Ok(exit.code());
    }

    // 普通 run：在 clear.lock SH 临界区内做归档预检并捕获 `(generation, last_seq)` 原子
    // 水位，再发布 own；随后**释放 SH** 才抢 runner.lock（避免与 `--clear` 的 EX 死锁）。
    // 这样 follower 的基线不会与 clear 的归档交错，也不会把首次建库误判为归档（B1/B2）。
    xidong::store::cleanup_inbox_tmp(&resolved.session, std::time::Duration::from_secs(3600));
    xidong::store::cleanup_spool_tmp(&resolved.session, std::time::Duration::from_secs(3600));
    let own = Record::user(prompt);
    let own_id = own.id().to_owned();
    let watermark = {
        let _clear_guard = xidong::store::FileLock::shared(&resolved.session.clear_lock_path())
            .map_err(anyhow::Error::from)?;
        if xidong::store::is_archived(&resolved.session).map_err(anyhow::Error::from)? {
            eprintln!(
                "错误：会话已归档，不可写入：{}",
                resolved.session.db.display()
            );
            return Ok(ExitCode::Error.code());
        }
        let watermark = xidong::store::watermark(&resolved.session).map_err(anyhow::Error::from)?;
        // 同一 SH 临界区内的发布再带一次 generation 守卫：即使将来有代码把发布挪出
        // 临界区，也不会把 own 写进已归档的新 generation（与 notify 共用同一语义）。
        xidong::store::publish_with_generation(
            &resolved.session,
            xidong::store::InboxPhase::Pending,
            &own,
            Some(watermark.generation),
        )
        .map_err(anyhow::Error::from)?;
        watermark
    };

    spawn_interrupt_handler();
    let exit = match xidong::store::FileLock::try_exclusive(&resolved.session.runner_lock_path())
        .map_err(anyhow::Error::from)?
    {
        Some(_runner_lock) => {
            let client = client.clone();
            runner
                .run_queued(
                    own,
                    xidong::core::agent::PendingPolicy::Abandon,
                    move |cfg| xidong::core::agent::OpenAiModel { cfg, client },
                )
                .await?
        }
        None => {
            let client = client.clone();
            runner
                .follow(own, watermark.last_seq, watermark.generation, move |cfg| {
                    xidong::core::agent::OpenAiModel { cfg, client }
                })
                .await?
        }
    };

    // stdout 只承载「成功的最终助手文本」：失败回合（模型/配置/用法错误）的原因已由
    // stderr 报告，若再打印上一条历史回复会把过期答案伪装成本次结果。
    // `--json` 下最终回复已作为 `text` 事件进入 JSONL，不再单独打印。
    print_outcome(exit, &cli, &stderr_sink, sink, &config, || {
        print_final_ai_for_own(&resolved.session, &own_id)
    })?;

    Ok(exit.code())
}

/// 启动 Ctrl-C 处理任务：向当前工具进程组转发 SIGINT，宽限内仍存活则 SIGKILL 兜底，
/// 再以 130 退出。无工具在运行时（模型请求中）直接退出。
fn spawn_interrupt_handler() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            xidong::exec::interrupt_current_group(std::time::Duration::from_secs(2)).await;
            std::process::exit(130);
        }
    });
}

/// 按退出码决定是否打印最终回复（`Success`/`MaxSteps` 且非 `--json`）。
///
/// `print_final` 在需要打印时被调用一次；失败回合不打印，避免用历史回复冒充本次结果。
fn print_outcome(
    exit: ExitCode,
    cli: &Cli,
    stderr_sink: &StderrSink,
    sink: &dyn Sink,
    config: &xidong::core::config::Config,
    print_final: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match exit {
        ExitCode::Success => {
            if !cli.json {
                stderr_sink.finish_transcript();
                print_final()?;
            }
        }
        ExitCode::MaxSteps => {
            sink.emit(DisplayItem::Notice(format!(
                "警告：达到最大步数 {}，已停止；以下为当前最终回复。",
                config.agent.max_steps
            )));
            if !cli.json {
                stderr_sink.finish_transcript();
                print_final()?;
            }
        }
        // 其他退出码：stdout 保持干净。
        _ => {}
    }
    Ok(())
}

/// 打印 `seq > watermark` 的记录中**最后一条** `ai` 的内容；为空则不打印。
///
/// 只看最后一条 `ai`（而非最后一条非空 `ai`），否则末步为空回复时会把本轮中间
/// 步骤的正文当成本次结果打到 stdout（A5）。水位是记录的 `seq`，不是记录下标。
fn print_final_ai(session: &xidong::store::Session, watermark: i64) -> anyhow::Result<()> {
    let (rows, _) = xidong::store::read_after(session, watermark)?;
    if let Some((_, Record::Ai(ai))) = rows
        .iter()
        .rev()
        .find(|(_, record)| matches!(record, Record::Ai(_)))
        && !ai.content.is_empty()
    {
        println!("{}", ai.content);
    }
    Ok(())
}

/// 打印 `own_id` 之后**第一条轮末 `ai`** 的内容；为空或未落库则不打印。
///
/// 最终回复归属：多客户端合并一轮时，按自己的 user record id 定位答案，避免打印他人
/// 或历史答案（A5）。leader 与 follower 共用。
fn print_final_ai_for_own(session: &xidong::store::Session, own_id: &str) -> anyhow::Result<()> {
    let records = xidong::store::load(session)?;
    if let Some(content) = xidong::core::display::final_ai_for_own(&records, own_id)
        && !content.is_empty()
    {
        println!("{content}");
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

/// 用户主目录，仅作为未设置 `XIDONG_*`/`XDG_*` 时的路径回退。
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
            "XIDONG_NOTIFY_TIMEOUT_SECS" => Some(" 7 ".to_string()),
            "XIDONG_NOTIFY_HEAD_CHARS" => Some("abc".to_string()),
            _ => None,
        };
        assert_eq!(
            parse_env::<u64>(&env, "XIDONG_NOTIFY_TIMEOUT_SECS"),
            Some(7)
        );
        assert_eq!(parse_env::<usize>(&env, "XIDONG_NOTIFY_HEAD_CHARS"), None);
        assert_eq!(parse_env::<u64>(&env, "MISSING"), None);
    }

    #[test]
    fn run_session_uses_env_only_for_approve() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        let env_path = dir.path().join("from-env.db");
        let env =
            |key: &str| (key == "XIDONG_SESSION").then(|| env_path.to_string_lossy().into_owned());

        // C1：`--approve` 认 XIDONG_SESSION，与 `--notify` 解析链一致。
        let approve = Cli::parse_from(["xidong", "--approve"]);
        let resolved = resolve_run_session(&approve, &env, &data_root).unwrap();
        assert_eq!(
            resolved.session.db,
            env_path,
            "--approve 应认 XIDONG_SESSION：{}",
            resolved.session.db.display()
        );

        // `--session` 优先于 XIDONG_SESSION。
        let explicit = dir.path().join("explicit.db");
        let approve = Cli::parse_from([
            "xidong",
            "--approve",
            "--session",
            explicit.to_str().unwrap(),
        ]);
        let resolved = resolve_run_session(&approve, &env, &data_root).unwrap();
        assert_eq!(resolved.session.db, explicit, "--session 应最优先");

        // 普通 run 的会话解析保持不变：忽略 XIDONG_SESSION，仍走 cwd 默认。
        let normal = Cli::parse_from(["xidong", "你好"]);
        let resolved = resolve_run_session(&normal, &env, &data_root).unwrap();
        assert!(
            resolved.session.root.starts_with(&data_root),
            "普通 run 不得因 XIDONG_SESSION 改变目标：{}",
            resolved.session.db.display()
        );
    }

    #[test]
    fn status_counts_include_approval_and_sum_to_total() {
        let records = vec![
            Record::user("你好"),
            Record::ai("model", "hash", "答", None),
            Record::tool_call(xidong::msg::ToolOrigin::Ai, "c1", "echo hi", None),
            Record::tool_result("c1", "hi\n", "", 0, false, false, None),
            Record::system("notify", "x", None, None),
            Record::approval_pending("c1", "echo hi", "echo *"),
            Record::approval_resolved("c1", xidong::msg::ApprovalDecision::Approve),
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
