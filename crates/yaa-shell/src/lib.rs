//! YAA 的唯一工具：Shell。
//!
//! 执行 `sh -c <command>`，`stdin` 置空；超时对整进程组 `SIGKILL`；stdout/stderr
//! 并发流式读取并在超过阈值时截断，完整输出落盘。
//!
//! 仅支持 POSIX；非 Unix 平台返回错误。

mod exec;
mod sink;

use std::path::{Path, PathBuf};

/// 截断与超时限制。
#[derive(Debug, Clone)]
pub struct ShellLimits {
    pub timeout_secs: u64,
    pub max_stream_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
}

/// 一次工具调用请求。
#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub call_id: String,
    pub command: String,
}

/// 一次工具调用结果。
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

/// Shell 执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("Shell 任务失败：{0}")]
    Join(String),
    #[error("Shell 工具仅支持 POSIX 系统")]
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
}
