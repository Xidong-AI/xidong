//! 进程执行：POSIX 进程组、超时整组击杀、双流并发读取。
//!
//! 读取线程通过 channel 交付结果，主线程只用带截止期的 `recv_timeout` 等待，因此
//! 无论后台进程是否攥住管道，`run_blocking` 都保证在硬上限内返回。

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::sink::{FullOutput, Sink, StreamOutput};
use crate::{ShellCommand, ShellError, ShellLimits, ShellOutput};

/// 直接子进程退出后，等待两个读取线程交付结果的宽限期。
// PJ568 方案：这是读取线程等待的硬上限。代价是「逃逸出本进程组、仍持有
// stdout/stderr 管道」的进程输出会被放弃（丢失），换取向调用方保证永不挂起。
// 升级路径：若需回收这类输出，可给读取端保留一份可主动 close 的管道副本以强制
// 解除阻塞，或用 pidfd + 超时轮询逐个回收逃逸进程，再读尽管道。
const READER_GRACE: Duration = Duration::from_secs(2);

/// 首次宽限到期后再次 `killpg`，再给读取线程的短宽限期。
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(1);

/// 等待直接子进程被回收的硬上限（`killpg` 之后的兜底）。
const REAP_GRACE: Duration = Duration::from_secs(2);

/// 进程无法在宽限期内回收时的最终等待。
const REAP_FINAL_GRACE: Duration = Duration::from_secs(1);

#[cfg(unix)]
pub(crate) fn run_blocking(
    cmd: &ShellCommand,
    limits: &ShellLimits,
    output_dir: &Path,
) -> Result<ShellOutput, ShellError> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd.command)
        .envs(cmd.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 让子进程自建进程组，超时时可对整组击杀，避免孙进程持有管道。
        command.process_group(0);
    }

    let mut child = command.spawn()?;
    let pgid = child.id() as i32;

    let stdout = child
        .stdout
        .take()
        .ok_or(ShellError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ShellError::MissingPipe("stderr"))?;
    let full_out = Arc::new(FullOutput::new(output_dir, "stdout"));
    let full_err = Arc::new(FullOutput::new(output_dir, "stderr"));

    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    {
        let limits = limits.clone();
        let full = Arc::clone(&full_out);
        thread::spawn(move || {
            let _ = tx_out.send(Sink::run(stdout, &limits, &full));
        });
    }
    {
        let limits = limits.clone();
        let full = Arc::clone(&full_err);
        thread::spawn(move || {
            let _ = tx_err.send(Sink::run(stderr, &limits, &full));
        });
    }

    let deadline = Duration::from_secs(limits.timeout_secs);
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= deadline {
            timed_out = true;
            kill_group(pgid, &mut child);
            break wait_reaped(&mut child)?;
        }
        thread::sleep(Duration::from_millis(10));
    };

    // 读取线程与结果汇总：不论后台进程是否攥住管道，等待都受截止期约束。
    let grace_deadline = Instant::now() + READER_GRACE;
    let mut out_result = recv_until(&rx_out, grace_deadline);
    let mut err_result = recv_until(&rx_err, grace_deadline);
    if out_result.is_none() || err_result.is_none() {
        // 读取线程仍被管道卡住：再杀一次进程组，给最后一个短宽限。
        kill_group(pgid, &mut child);
        let final_deadline = Instant::now() + READER_GRACE_AFTER_KILL;
        if out_result.is_none() {
            out_result = recv_until(&rx_out, final_deadline);
        }
        if err_result.is_none() {
            err_result = recv_until(&rx_err, final_deadline);
        }
    }

    let (stdout, stdout_abandoned) = resolve_stream(out_result, "stdout")?;
    let (stderr, stderr_abandoned) = resolve_stream(err_result, "stderr")?;
    if stdout_abandoned || stderr_abandoned {
        timed_out = true;
    }

    let truncated = stdout.truncated || stderr.truncated;
    let canonical = |path: Option<PathBuf>| path.map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    let stdout_file = canonical(full_out.path());
    let stderr_file = canonical(full_err.path());

    Ok(ShellOutput {
        call_id: cmd.call_id.clone(),
        stdout: stdout.text,
        stderr: stderr.text,
        code: status.code().unwrap_or(-1),
        timed_out,
        truncated,
        stdout_file,
        stderr_file,
    })
}

/// 在截止期前接收一个流的读取结果；超时或线程异常结束都返回 `None`，绝不阻塞。
#[cfg(unix)]
fn recv_until(
    rx: &Receiver<io::Result<StreamOutput>>,
    deadline: Instant,
) -> Option<io::Result<StreamOutput>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(remaining).ok()
}

/// 把读取结果归一为 `StreamOutput`：未交付则视为空流并给出清晰提示。
#[cfg(unix)]
fn resolve_stream(
    result: Option<io::Result<StreamOutput>>,
    name: &str,
) -> Result<(StreamOutput, bool), ShellError> {
    match result {
        Some(Ok(stream)) => Ok((stream, false)),
        Some(Err(e)) => Err(e.into()),
        None => Ok((
            StreamOutput {
                text: format!("[{name} 读取超时：已放弃等待持有管道的后台进程]"),
                truncated: false,
            },
            true,
        )),
    }
}

/// 对整进程组发送 SIGKILL；失败时退回直接杀子进程兜底。
#[cfg(unix)]
fn kill_group(pgid: i32, child: &mut Child) {
    // SAFETY: `pgid` 是本进程 `spawn` 的子进程组 id（`process_group(0)`），
    // 调用 `killpg` 向整组发送 SIGKILL 是唯一目的；参数有效，无内存影响。
    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if rc != 0 {
        // 进程组可能已不存在或权限异常：直接对子进程兜底。
        let _ = child.kill();
    }
}

/// 有界回收直接子进程，替代可能永久阻塞的 `wait()`。
#[cfg(unix)]
fn wait_reaped(child: &mut Child) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let final_deadline = Instant::now() + REAP_FINAL_GRACE;
            while Instant::now() < final_deadline {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                thread::sleep(Duration::from_millis(10));
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "子进程在宽限期内未能回收",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(unix))]
pub(crate) fn run_blocking(
    _cmd: &ShellCommand,
    _limits: &ShellLimits,
    _output_dir: &Path,
) -> Result<ShellOutput, ShellError> {
    Err(ShellError::Unsupported)
}
