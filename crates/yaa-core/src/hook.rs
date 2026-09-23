//! 钩子执行。
//!
//! 钩子以 `sh -c <command>` 运行，`stdin` 置空、stdout/stderr 管道读取，每个流有
//! 独立 1 MiB 硬上限：超限立即终止进程并报错，绝不静默截断（钩子输出可能承载配置
//! 或提示词，静默裁掉会破坏语义）。
//!
//! 不依赖 `yaa-exec`：Shell 工具的流式截断策略与钩子的「超限即错」策略不同。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// 单流输出上限（1 MiB）。超过即报错，不截断。
const MAX_STREAM_BYTES: usize = 1024 * 1024;

/// `killpg` 之后回收直接子进程的兜底宽限期。
const REAP_GRACE: Duration = Duration::from_secs(2);

/// 一次钩子执行的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// 钩子执行错误。
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("启动钩子失败：{0}")]
    Spawn(#[from] std::io::Error),
    #[error("钩子 {stream} 输出超过 {MAX_STREAM_BYTES} 字节上限，已终止（钩子输出不会被截断）")]
    TooLarge { stream: &'static str },
    #[error("钩子子进程缺少 {0} 管道")]
    MissingPipe(&'static str),
    #[error("钩子执行超过 {secs} 秒超时，已终止整个进程组。")]
    Timeout { secs: u64 },
}

type Result<T> = std::result::Result<T, HookError>;

/// 执行钩子命令。
///
/// 硬性约束：从 `spawn` 到读尽两个流的**整个**过程都受 `timeout_secs` 约束。
/// `sh` 以独立进程组启动，超时时对**整组**发 SIGKILL，因此即使钩子留下的后台子进程
/// 仍攥着管道，`run` 也会在截止期内返回 `HookError::Timeout`，绝不永久挂起。
pub async fn run(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_secs: u64,
) -> Result<HookOutcome> {
    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // 让 `sh` 自建进程组，超时时可击杀整组（含攥住管道的孙进程）。
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    // `process_group(0)` 下子进程组 id == 子进程 pid。
    let pid = child.id();

    let stdout = child
        .stdout
        .take()
        .ok_or(HookError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(HookError::MissingPipe("stderr"))?;

    let drain_deadline = deadline.saturating_sub(start.elapsed());
    let (out, err) = match tokio::time::timeout(drain_deadline, drain_both(stdout, stderr)).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => {
            // 超限时 `drain_both` 已停止读取；必须杀死进程组，避免其阻塞在写管道上。
            kill_group(pid, &mut child);
            reap(&mut child).await;
            return Err(err);
        }
        Err(_) => {
            // 进程组里可能有攥住管道、永不 EOF 的后台子进程：整组击杀后立即返回。
            kill_group(pid, &mut child);
            reap(&mut child).await;
            return Err(HookError::Timeout { secs: timeout_secs });
        }
    };

    // 管道已 EOF，但子进程可能尚未退出；再以剩余预算回收，整体仍不越过截止期。
    let wait_budget = deadline
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(50));
    match tokio::time::timeout(wait_budget, child.wait()).await {
        Ok(status) => Ok(HookOutcome {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            code: status?.code().unwrap_or(-1),
        }),
        Err(_) => {
            kill_group(pid, &mut child);
            reap(&mut child).await;
            Err(HookError::Timeout { secs: timeout_secs })
        }
    }
}

/// 对整进程组发送 SIGKILL，并兜底直接杀子进程。
fn kill_group(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `pid` 是 `process_group(0)` 建立的子进程组 id，向整组发 SIGKILL
        // 是唯一目的；参数有效，无内存影响。
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
    // 进程组可能已不存在或非 unix：直接对子进程兜底。
    let _ = child.start_kill();
}

/// 有界回收直接子进程，绝不永久阻塞。
async fn reap(child: &mut tokio::process::Child) {
    let _ = tokio::time::timeout(REAP_GRACE, child.wait()).await;
}

/// 并发读取两个流，任一流超过上限即返回错误。
async fn drain_both(
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
) -> Result<(Vec<u8>, Vec<u8>)> {
    use tokio::io::AsyncReadExt;

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];

    while !out_done || !err_done {
        tokio::select! {
            read = stdout.read(&mut out_buf), if !out_done => {
                let n = read?;
                if n == 0 {
                    out_done = true;
                } else {
                    out.extend_from_slice(&out_buf[..n]);
                    if out.len() > MAX_STREAM_BYTES {
                        return Err(HookError::TooLarge { stream: "stdout" });
                    }
                }
            }
            read = stderr.read(&mut err_buf), if !err_done => {
                let n = read?;
                if n == 0 {
                    err_done = true;
                } else {
                    err.extend_from_slice(&err_buf[..n]);
                    if err.len() > MAX_STREAM_BYTES {
                        return Err(HookError::TooLarge { stream: "stderr" });
                    }
                }
            }
        }
    }
    Ok((out, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常规测试用的宽松超时。
    const TEST_TIMEOUT: u64 = 30;

    fn empty_env() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[tokio::test]
    async fn captures_stdout_and_code() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("echo hello", dir.path(), &empty_env(), TEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(outcome.stdout, "hello\n");
        assert_eq!(outcome.code, 0);
    }

    #[tokio::test]
    async fn captures_nonzero_code_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(
            "echo oops >&2; exit 3",
            dir.path(),
            &empty_env(),
            TEST_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(outcome.code, 3);
        assert_eq!(outcome.stderr, "oops\n");
    }

    #[tokio::test]
    async fn passes_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = empty_env();
        env.insert("GLB_CFG".to_string(), "/cfg".to_string());
        let outcome = run("printf '%s' \"$GLB_CFG\"", dir.path(), &env, TEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(outcome.stdout, "/cfg");
    }

    #[tokio::test]
    async fn oversized_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // 输出 2 MiB：超过上限必须报错而非截断。
        let outcome = run(
            "head -c 2097152 /dev/zero",
            dir.path(),
            &empty_env(),
            TEST_TIMEOUT,
        )
        .await;
        assert!(matches!(outcome, Err(HookError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn hanging_hook_times_out_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = run("sleep 30", dir.path(), &empty_env(), 1).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(HookError::Timeout { secs: 1 })),
            "应返回 Timeout：{outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "超时应及时返回：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn background_child_holding_pipes_times_out_promptly() {
        // `sh` 立即退出，但后台 `sleep` 继承并攥住 stdout/stderr 管道：
        // 直接子进程的 kill 不够，必须整组击杀才不会永久挂起。
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = run("sleep 30 &", dir.path(), &empty_env(), 1).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(HookError::Timeout { secs: 1 })),
            "应返回 Timeout：{outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "超时应及时返回：{elapsed:?}"
        );
    }
}
