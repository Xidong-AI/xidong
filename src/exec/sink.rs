//! 有界、截断的流读取。
//!
//! 每个流（stdout/stderr）独立计数：总字符数未超过 `max_stream_chars` 时全量
//! 缓冲；一旦超过，标记截断、把已缓冲的原始字节刷入完整输出文件，此后内存中只
//! 保留头 `head_chars` 与滚动尾 `tail_chars`，其余字节直接落盘。
//!
//! 字符按 Unicode 标量值计数；入记录文本做 lossy UTF-8，落盘保留原始字节。

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::exec::ShellLimits;

/// 完整输出文件，懒创建。每个流持有独立实例，避免两流内容无标签地交织。
///
/// 文件名由本地生成的 ULID 加流后缀决定，绝不使用模型提供的 `call_id`，避免路径
/// 穿越与覆盖。
pub(crate) struct FullOutput {
    dir: PathBuf,
    suffix: &'static str,
    inner: Mutex<Option<(PathBuf, File)>>,
}

impl FullOutput {
    pub(crate) fn new(dir: &Path, suffix: &'static str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            suffix,
            inner: Mutex::new(None),
        }
    }

    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            fs::create_dir_all(&self.dir)?;
            let path = self
                .dir
                .join(format!("{}.{}.out", ulid::Ulid::new(), self.suffix));
            let file = File::create(&path)?;
            *guard = Some((path, file));
        }
        if let Some((_, file)) = guard.as_mut() {
            file.write_all(bytes)?;
        }
        Ok(())
    }

    /// 完整输出文件路径；未发生截断时为 `None`。
    pub(crate) fn path(&self) -> Option<PathBuf> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.as_ref().map(|(path, _)| path.clone())
    }
}

/// 单个流的读取结果。
pub(crate) struct StreamOutput {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

enum State {
    Buffering {
        raw: Vec<u8>,
        text: String,
        count: usize,
    },
    Truncated {
        head: String,
        tail: VecDeque<char>,
    },
}

/// 有界截断 sink。
pub(crate) struct Sink {
    max_chars: usize,
    head_chars: usize,
    tail_chars: usize,
    decoder: Utf8Decoder,
    state: State,
    truncated: bool,
}

impl Sink {
    pub(crate) fn new(limits: &ShellLimits) -> Self {
        Self {
            max_chars: limits.max_stream_chars,
            head_chars: limits.head_chars,
            tail_chars: limits.tail_chars,
            decoder: Utf8Decoder::default(),
            state: State::Buffering {
                raw: Vec::new(),
                text: String::new(),
                count: 0,
            },
            truncated: false,
        }
    }

    /// 持续读取直到 EOF，返回入记录所需的文本与截断标记。
    pub(crate) fn run<R: Read>(
        mut reader: R,
        limits: &ShellLimits,
        full: &FullOutput,
    ) -> io::Result<StreamOutput> {
        let mut sink = Sink::new(limits);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => sink.feed(&buf[..n], full)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        sink.finish();
        Ok(sink.into_output())
    }

    fn feed(&mut self, bytes: &[u8], full: &FullOutput) -> io::Result<()> {
        if let State::Buffering { raw, text, count } = &mut self.state {
            raw.extend_from_slice(bytes);
            *count += self.decoder.push(bytes, text);
            if *count > self.max_chars {
                full.write_all(raw)?;
                self.truncated = true;
                let (head, tail) = split_head_tail(text, self.head_chars, self.tail_chars);
                self.state = State::Truncated { head, tail };
            }
            return Ok(());
        }

        full.write_all(bytes)?;
        let mut newly = String::new();
        self.decoder.push(bytes, &mut newly);
        if let State::Truncated { tail, .. } = &mut self.state {
            push_tail(tail, &newly, self.tail_chars);
        }
        Ok(())
    }

    fn finish(&mut self) {
        let mut trailing = String::new();
        self.decoder.finish(&mut trailing);
        if trailing.is_empty() {
            return;
        }
        match &mut self.state {
            State::Buffering { text, .. } => text.push_str(&trailing),
            State::Truncated { tail, .. } => push_tail(tail, &trailing, self.tail_chars),
        }
    }

    fn into_output(self) -> StreamOutput {
        let text = match self.state {
            State::Buffering { text, .. } => text,
            State::Truncated { head, tail } => {
                let tail: String = tail.into_iter().collect();
                format!("{head}\n... [truncated] ...\n{tail}")
            }
        };
        StreamOutput {
            text,
            truncated: self.truncated,
        }
    }
}

fn split_head_tail(text: &str, head_chars: usize, tail_chars: usize) -> (String, VecDeque<char>) {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars.iter().take(head_chars).collect();
    let tail_start = chars.len().saturating_sub(tail_chars);
    let tail: VecDeque<char> = chars[tail_start..].iter().copied().collect();
    (head, tail)
}

fn push_tail(tail: &mut VecDeque<char>, text: &str, tail_chars: usize) {
    for ch in text.chars() {
        tail.push_back(ch);
    }
    while tail.len() > tail_chars {
        tail.pop_front();
    }
}

/// 增量 UTF-8 解码器：跨读取块保留不完整序列，非法序列记为 U+FFFD。
#[derive(Default)]
struct Utf8Decoder {
    carry: Vec<u8>,
}

impl Utf8Decoder {
    /// 把 `bytes` 解码追加到 `out`，返回新增的字符数。
    fn push(&mut self, bytes: &[u8], out: &mut String) -> usize {
        let mut added = 0;
        self.carry.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.carry) {
                Ok(valid) => {
                    added += valid.chars().count();
                    out.push_str(valid);
                    self.carry.clear();
                    break;
                }
                Err(err) => {
                    let valid_len = err.valid_up_to();
                    let error_len = err.error_len();
                    if valid_len > 0 {
                        let prefix =
                            std::str::from_utf8(&self.carry[..valid_len]).expect("有效前缀");
                        added += prefix.chars().count();
                        out.push_str(prefix);
                    }
                    match error_len {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            added += 1;
                            self.carry.drain(..valid_len + len);
                        }
                        None => {
                            self.carry.drain(..valid_len);
                            break;
                        }
                    }
                }
            }
        }
        added
    }

    /// 结束时把残留的不完整序列记为替换字符。
    fn finish(&mut self, out: &mut String) {
        if !self.carry.is_empty() {
            out.push('\u{FFFD}');
            self.carry.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decoder_handles_split_sequences() {
        let mut decoder = Utf8Decoder::default();
        let mut out = String::new();
        // "中" = E4 B8 AD，拆成两段。
        decoder.push(&[0xE4, 0xB8], &mut out);
        assert!(out.is_empty());
        let added = decoder.push(&[0xAD], &mut out);
        assert_eq!(out, "中");
        assert_eq!(added, 1);
    }

    #[test]
    fn utf8_decoder_marks_invalid_bytes() {
        let mut decoder = Utf8Decoder::default();
        let mut out = String::new();
        decoder.push(&[0xFF, b'a'], &mut out);
        assert_eq!(out, "\u{FFFD}a");
    }

    #[test]
    fn utf8_decoder_finish_flushes_partial() {
        let mut decoder = Utf8Decoder::default();
        let mut out = String::new();
        decoder.push(&[0xE4, 0xB8], &mut out);
        decoder.finish(&mut out);
        assert_eq!(out, "\u{FFFD}");
    }

    #[test]
    fn push_tail_keeps_rolling_window() {
        let mut tail = VecDeque::new();
        push_tail(&mut tail, "abcde", 3);
        assert_eq!(tail.into_iter().collect::<String>(), "cde");
    }
}
