//! YAA 的 Shell 执行机制。
//!
//! 不含工具语义（schema、参数解析、错误文案与调用编排在 `yaa-core::tools::shell`）。
//! 执行 `sh -c <command>`，`stdin` 置空；超时对整进程组 `SIGKILL`；stdout/stderr
//! 并发流式读取并在超过阈值时截断，完整输出落盘。
//!
//! 仅支持 POSIX；非 Unix 平台返回错误。

mod exec;
mod sink;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

/// 当前正在执行的工具进程组 id；0 表示没有工具在运行（如模型请求期间）。
///
/// 进程级单例：同一时刻 agent loop 只执行一个工具，故无需额外同步结构。
static CURRENT_PGID: AtomicI32 = AtomicI32::new(0);

/// 发布当前工具进程组 id，供信号处理路径转发中断。
pub(crate) fn publish_group(pgid: i32) {
    CURRENT_PGID.store(pgid, Ordering::SeqCst);
}

/// 清除当前工具进程组 id（执行结束或发起前）。
pub(crate) fn clear_group() {
    CURRENT_PGID.store(0, Ordering::SeqCst);
}

/// 读取当前工具进程组 id；0 表示没有工具在运行。
pub fn current_group() -> i32 {
    CURRENT_PGID.load(Ordering::SeqCst)
}

/// 向当前工具进程组先发 SIGINT，宽限内仍存活则 SIGKILL 整组。
///
/// 返回是否存在需要中断的进程组（无工具运行时返回 `false`）。全程不阻塞事件循环，
/// 以约 20ms 间隔探测组是否消失。
pub async fn interrupt_current_group(grace: std::time::Duration) -> bool {
    let pgid = current_group();
    if pgid == 0 {
        return false;
    }

    // SAFETY: `pgid` 由 `exec` 在 spawn 后以 `process_group(0)` 的子进程 pid 发布，
    // 是有效的进程组 id；`killpg` 只向其发送信号，无内存影响。
    unsafe {
        libc::killpg(pgid, libc::SIGINT);
    }

    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        // SAFETY: 同上；信号 0 只做存在性探测，不投递任何信号。
        let gone = unsafe { libc::killpg(pgid, 0) } != 0;
        if gone {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // 宽限后仍存活：整组 SIGKILL 兜底，避免留下无人监管的孤儿。
    // SAFETY: 同上；对已不存在的进程组发 SIGKILL 是安全的 no-op（返回 ESRCH）。
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    true
}

/// 截断与超时限制。
#[derive(Debug, Clone)]
pub struct ShellLimits {
    pub timeout_secs: u64,
    pub max_stream_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
}

/// 一次 Shell 执行请求。
#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub call_id: String,
    pub command: String,
    /// 额外注入子进程的环境变量（如 `YAA_SESSION`），在继承的父环境之上覆盖。
    pub env: Vec<(String, String)>,
}

/// 一次 Shell 执行结果。
#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub call_id: String,
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub truncated: bool,
    /// 仅当 stdout 被截断时给出，绝对路径，指向完整 stdout。
    pub stdout_file: Option<PathBuf>,
    /// 仅当 stderr 被截断时给出，绝对路径，指向完整 stderr。
    pub stderr_file: Option<PathBuf>,
}

/// 一个被捕获的流（如 stdin）的截断结果与完整输出文件。
///
/// 没有 `truncated` 字段：是否截断可由 `file` 是否为 `Some` 唯一判定（只有截断才
/// 会落盘完整输入），保留独立布尔值只会产生两处可互相矛盾的状态。
#[derive(Debug, Clone)]
pub struct CapturedStream {
    /// 入记录文本：未截断为全量，截断为头尾拼接并带 `... [truncated] ...` 标记。
    pub text: String,
    /// 仅在发生截断时给出，绝对路径，指向完整输入。
    pub file: Option<PathBuf>,
}

/// Shell 执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("Shell 任务失败：{0}")]
    Join(String),
    #[error("Shell 执行仅支持 POSIX 系统")]
    Unsupported,
    #[error("子进程缺少管道：{0}")]
    MissingPipe(&'static str),
}

type Result<T> = std::result::Result<T, ShellError>;

/// 执行命令。阻塞逻辑在 `spawn_blocking` 中运行。
pub async fn run(
    cmd: &ShellCommand,
    limits: &ShellLimits,
    output_dir: &Path,
) -> Result<ShellOutput> {
    let cmd = cmd.clone();
    let limits = limits.clone();
    let output_dir = output_dir.to_path_buf();
    tokio::task::spawn_blocking(move || exec::run_blocking(&cmd, &limits, &output_dir))
        .await
        .map_err(|e| ShellError::Join(e.to_string()))?
}

/// 用与命令流相同的截断/落盘策略捕获任意 `Read`（如 stdin）。
///
/// 复用 [`sink::Sink`] 与 [`sink::FullOutput`]：内存中只保留头 `head_chars` 与尾
/// `tail_chars`，一旦超过 `max_stream_chars` 就把完整输入写入 `output_dir` 下的
/// `<ULID>.stdin.out` 并返回其绝对路径。阻塞读取在 `spawn_blocking` 中执行。
pub async fn capture_reader<R>(
    reader: R,
    limits: &ShellLimits,
    output_dir: &Path,
) -> Result<CapturedStream>
where
    R: std::io::Read + Send + 'static,
{
    let limits = limits.clone();
    let output_dir = output_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let full = sink::FullOutput::new(&output_dir, "stdin");
        let stream = sink::Sink::run(reader, &limits, &full)?;
        let file = full
            .path()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        Ok::<_, ShellError>(CapturedStream {
            text: stream.text,
            file,
        })
    })
    .await
    .map_err(|e| ShellError::Join(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn limits() -> ShellLimits {
        ShellLimits {
            timeout_secs: 10,
            max_stream_chars: 1000,
            head_chars: 100,
            tail_chars: 50,
        }
    }

    fn cmd(command: &str) -> ShellCommand {
        ShellCommand {
            call_id: "call_test".into(),
            command: command.into(),
            env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn truncates_large_output_and_writes_full_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&cmd("seq 1 100000"), &limits(), dir.path())
            .await
            .unwrap();

        assert!(out.truncated);
        assert!(out.stdout.chars().count() < 100_000);
        assert!(out.stdout.contains("\n... [truncated] ...\n"));
        assert!(out.stdout.starts_with("1\n2\n"));
        assert!(out.stdout.trim_end().ends_with("100000"));

        let file = out.stdout_file.expect("截断应产生完整 stdout 文件");
        assert!(file.is_absolute());
        assert!(file.exists());
        assert!(file.to_string_lossy().ends_with(".stdout.out"));
        let full = std::fs::read_to_string(&file).unwrap();
        assert!(full.starts_with("1\n2\n"));
        assert!(full.trim_end().ends_with("100000"));
        assert!(full.chars().count() > out.stdout.chars().count());
        assert!(out.stderr_file.is_none());
    }

    #[tokio::test]
    async fn both_streams_truncate_into_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &cmd("echo OUT; seq 1 100000; echo ERR >&2; seq 1 100000 >&2"),
            &limits(),
            dir.path(),
        )
        .await
        .unwrap();

        assert!(out.truncated);
        let stdout_file = out.stdout_file.expect("stdout 应截断");
        let stderr_file = out.stderr_file.expect("stderr 应截断");
        assert_ne!(stdout_file, stderr_file);

        let full_out = std::fs::read_to_string(&stdout_file).unwrap();
        let full_err = std::fs::read_to_string(&stderr_file).unwrap();
        assert!(full_out.starts_with("OUT\n"));
        assert!(full_err.starts_with("ERR\n"));
        assert!(!full_out.contains("ERR"));
        assert!(!full_err.contains("OUT"));
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut limits = limits();
        limits.timeout_secs = 1;

        let start = Instant::now();
        let out = run(&cmd("sleep 30"), &limits, dir.path()).await.unwrap();
        assert!(out.timed_out);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn timeout_kills_background_children() {
        let dir = tempfile::tempdir().unwrap();
        let mut limits = limits();
        limits.timeout_secs = 1;

        let start = Instant::now();
        let out = run(&cmd("sleep 30 & sleep 30"), &limits, dir.path())
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn background_child_holding_pipe_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut limits = limits();
        limits.timeout_secs = 1;

        // 后台子进程继承并攥住 stdout/stderr 管道，但直接子进程（sh）立即退出。
        // 调用必须在硬上限内返回，而不是等后台进程 30 秒后释放管道。
        let start = Instant::now();
        let out = run(&cmd("sleep 30 &"), &limits, dir.path()).await.unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "应在硬上限内返回，实际耗时 {:?}",
            start.elapsed()
        );
        let _ = out;
    }

    #[tokio::test]
    async fn escaped_process_group_output_is_abandoned_without_timeout_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut limits = limits();
        // 远大于读取宽限（2s + 1s），确保返回不是超时分支。
        limits.timeout_secs = 30;

        // `setsid` 让后台进程脱离本进程组并新建会话，仍继承 stdout/stderr 管道：
        // 直接子进程 sh 立即退出，但管道被攥住，读取在宽限期后被放弃。
        let start = Instant::now();
        let out = run(&cmd("setsid sleep 30 &"), &limits, dir.path())
            .await
            .unwrap();

        assert!(
            start.elapsed() < std::time::Duration::from_secs(6),
            "应在读取宽限内返回，实际耗时 {:?}",
            start.elapsed()
        );
        assert_eq!(out.code, 0, "直接子进程 sh 正常退出");
        assert!(!out.timed_out, "逃逸进程攥管道只放弃输出，不表示真的超时");
        assert!(
            out.stdout.contains("读取超时") || out.stderr.contains("读取超时"),
            "被放弃的流仍须以占位文本自解释：stdout={:?} stderr={:?}",
            out.stdout,
            out.stderr
        );
    }

    #[tokio::test]
    async fn stdin_is_null_so_cat_exits() {
        let dir = tempfile::tempdir().unwrap();
        let start = Instant::now();
        let out = run(&cmd("cat"), &limits(), dir.path()).await.unwrap();
        assert_eq!(out.code, 0);
        assert!(!out.timed_out);
        assert!(out.stdout.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn small_output_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&cmd("echo hi"), &limits(), dir.path()).await.unwrap();
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.code, 0);
        assert!(!out.truncated);
        assert!(out.stdout_file.is_none());
        assert!(out.stderr_file.is_none());
    }

    #[tokio::test]
    async fn capture_reader_small_input_has_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let captured = capture_reader(
            std::io::Cursor::new(b"hello\n".to_vec()),
            &limits(),
            dir.path(),
        )
        .await
        .unwrap();

        assert!(captured.file.is_none());
        assert_eq!(captured.text, "hello\n");
    }

    #[tokio::test]
    async fn capture_reader_truncates_and_writes_full_input() {
        let dir = tempfile::tempdir().unwrap();
        let input: String = (1..=100_000).map(|n| format!("{n}\n")).collect();

        let captured = capture_reader(
            std::io::Cursor::new(input.clone().into_bytes()),
            &limits(),
            dir.path(),
        )
        .await
        .unwrap();

        assert!(captured.text.contains("\n... [truncated] ...\n"));
        assert!(captured.text.starts_with("1\n2\n"));
        assert!(captured.text.trim_end().ends_with("100000"));
        assert!(captured.text.chars().count() < input.chars().count());

        let file = captured.file.expect("截断应落盘完整输入");
        assert!(file.is_absolute());
        assert!(file.to_string_lossy().ends_with(".stdin.out"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            input,
            "完整输出文件应保留全部原始输入"
        );
    }
}
