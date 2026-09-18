//! 进程执行：POSIX 进程组、超时整组击杀、双流并发读取。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::sink::{FullOutput, Sink};
use crate::{ShellCommand, ShellError, ShellLimits, ShellOutput};

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

    let stdout = child.stdout.take().ok_or(ShellError::MissingPipe("stdout"))?;
    let stderr = child.stderr.take().ok_or(ShellError::MissingPipe("stderr"))?;
    let full_out = Arc::new(FullOutput::new(output_dir, "stdout"));
    let full_err = Arc::new(FullOutput::new(output_dir, "stderr"));

    let out_handle = {
        let limits = limits.clone();
        let full = Arc::clone(&full_out);
        thread::spawn(move || Sink::run(stdout, &limits, &full))
    };
    let err_handle = {
        let limits = limits.clone();
        let full = Arc::clone(&full_err);
        thread::spawn(move || Sink::run(stderr, &limits, &full))
    };

    let deadline = Duration::from_secs(limits.timeout_secs);
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= deadline {
            timed_out = true;
            // SAFETY: `pgid` 是本进程 `spawn` 的子进程组 id（`process_group(0)`），
            // 调用 `killpg` 向整组发送 SIGKILL 是唯一目的；参数有效，无内存影响。
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout = out_handle
        .join()
        .map_err(|_| ShellError::Join("stdout 读取线程 panic".into()))??;
    let stderr = err_handle
        .join()
        .map_err(|_| ShellError::Join("stderr 读取线程 panic".into()))??;

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

#[cfg(not(unix))]
pub(crate) fn run_blocking(
    _cmd: &ShellCommand,
    _limits: &ShellLimits,
    _output_dir: &Path,
) -> Result<ShellOutput, ShellError> {
    Err(ShellError::Unsupported)
}
