//! 钩子执行。
//!
//! 钩子以 `sh -c <command>` 运行，`stdin` 置空、stdout/stderr 管道读取，每个流有
//! 独立 1 MiB 硬上限：超限立即终止进程并报错，绝不静默截断（提示词钩子的输出即
//! 完整提示词，静默裁掉会破坏语义）。
//!
//! 不依赖 `yaa-shell`：Shell 工具的流式截断策略与钩子的「超限即错」策略不同。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use sha2::{Digest, Sha256};

/// 单流输出上限（1 MiB）。超过即报错，不截断。
const MAX_STREAM_BYTES: usize = 1024 * 1024;

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
}

type Result<T> = std::result::Result<T, HookError>;

/// 执行钩子命令。
pub async fn run(command: &str, cwd: &Path, env: &BTreeMap<String, String>) -> Result<HookOutcome> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or(HookError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(HookError::MissingPipe("stderr"))?;

    let (out, err) = match drain_both(stdout, stderr).await {
        Ok(pair) => pair,
        Err(err) => {
            // 超限时 `drain_both` 已停止读取；必须杀死进程，避免其阻塞在写管道上。
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(err);
        }
    };

    let status = child.wait().await?;
    Ok(HookOutcome {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        code: status.code().unwrap_or(-1),
    })
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

/// 钩子缓存键的指纹：sha2-256 十六进制。
///
/// 输入为 `name \0 command \0 yaa 版本 \0 extra`，任何一项变化都会使缓存失效。
pub fn fingerprint(name: &str, command: &str, extra: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(command.as_bytes());
    hasher.update([0]);
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update([0]);
    hasher.update(extra.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_env() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[tokio::test]
    async fn captures_stdout_and_code() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("echo hello", dir.path(), &empty_env()).await.unwrap();
        assert_eq!(outcome.stdout, "hello\n");
        assert_eq!(outcome.code, 0);
    }

    #[tokio::test]
    async fn captures_nonzero_code_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("echo oops >&2; exit 3", dir.path(), &empty_env())
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
        let outcome = run("printf '%s' \"$GLB_CFG\"", dir.path(), &env)
            .await
            .unwrap();
        assert_eq!(outcome.stdout, "/cfg");
    }

    #[tokio::test]
    async fn oversized_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // 输出 2 MiB：超过上限必须报错而非截断。
        let outcome = run("head -c 2097152 /dev/zero", dir.path(), &empty_env()).await;
        assert!(matches!(outcome, Err(HookError::TooLarge { .. })));
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = fingerprint("llm", "cat llm.toml", "");
        let b = fingerprint("llm", "cat llm.toml", "");
        assert_eq!(a, b);
        assert_ne!(a, fingerprint("llm", "cat other.toml", ""));
        assert_ne!(a, fingerprint("system", "cat llm.toml", ""));
        assert_ne!(a, fingerprint("llm", "cat llm.toml", "extra"));
        assert_eq!(a.len(), 64);
    }
}
