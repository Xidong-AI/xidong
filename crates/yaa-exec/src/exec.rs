//! 进程执行：POSIX 进程组、超时整组击杀、双流并发读取。
//!
//! 读取线程通过 channel 交付结果，主线程只用带截止期的 `recv_timeout` 等待，因此
//! 无论后台进程是否攥住管道，`run_blocking` 都保证在硬上限内返回。
//!
//! `timed_out` 只在**真正超出时限**（`start.elapsed() >= deadline`）时置真；逃逸出
//! 本进程组、仍持有管道的后台进程会导致输出在读取宽限期后被放弃（回填占位提示），
//! 但这不等于超时，不置 `timed_out`。长任务应自行 `nohup <cmd> >/dev/null 2>&1 &`
//! 脱离工具管道，而不是依赖读取宽限。
//!
//! # 已知限制：被放弃的读取线程与管道 fd
//!
//! 放弃流时不会 join 读取线程（join 会与「永不挂起」的保证冲突）。这**不是永久
//! fd 泄漏**：读取线程阻塞在管道 `read` 上，只有当所有写端关闭（直接子进程与仍
//! 持有管道的逃逸进程都已退出）后 `read` 才返回 EOF，线程随之结束并释放其持有的
//! 管道 fd。也就是说，放弃只造成**暂时**占用；能被观测到的长期占用来源于把管道
//! 写端留在原地、永久存活的后台进程——这与放弃策略是同一取舍，并非额外的泄漏。

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
// 放弃**不**等价于超时：`timed_out` 只表示命令真的超出时限（见 `run_blocking`），
// 被放弃的流以占位文本自解释，调用方据此区分「超时击杀」与「后台进程攥管道」。
// 升级路径：若需回收这类输出，可给读取端保留一份可主动 close 的管道副本以强制
// 解除阻塞，或用 pidfd + 超时轮询逐个回收逃逸进程，再读尽管道。
const READER_GRACE: Duration = Duration::from_secs(2);

/// 首次宽限到期后再次 `killpg`，再给读取线程的短宽限期。
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(1);

/// 等待直接子进程被回收的硬上限（`killpg` 之后的兜底）。
const REAP_GRACE: Duration = Duration::from_secs(2);

/// 进程无法在宽限期内回收时的最终等待。
const REAP_FINAL_GRACE: Duration = Duration::from_secs(1);

/// 工具执行期间持有已发布的进程组 id，任何返回路径（含 `?` 早退）都清除发布。
// PJ568 方案：用 RAII 兜住 `run_blocking` 的多条返回路径，避免遗漏 `clear_group`
// 导致信号处理误向已结束的进程组发信号。
#[cfg(unix)]
struct GroupGuard;

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        crate::clear_group();
    }
}

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
    // 发布进程组 id 供 SIGINT 转发；`_group` 在函数任意返回路径上清除。
    crate::publish_group(pgid);
    let _group = GroupGuard;

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

    // 被放弃的流只回填占位文本，**不**把 `timed_out` 置真：逃逸进程组攥住管道并非
    // 超时，误报会让调用方以为命令被超时击杀（T5）。放弃的读取线程会在管道写端
    // 全部关闭后自行结束并释放 fd（见模块「已知限制」），无需也无法在此 join。
    let stdout = resolve_stream(out_result, "stdout")?;
    let stderr = resolve_stream(err_result, "stderr")?;

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
) -> Result<StreamOutput, ShellError> {
    match result {
        Some(Ok(stream)) => Ok(stream),
        Some(Err(e)) => Err(e.into()),
        None => Ok(StreamOutput {
            text: format!("[{name} 读取超时：已放弃等待持有管道的后台进程]"),
            truncated: false,
        }),
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
