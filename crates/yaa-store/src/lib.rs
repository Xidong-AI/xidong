//! YAA 会话 JSONL 同步存储。
//!
//! 追加以「整批一次 `write_all` + `sync_all`」保证不会出现半行交织；读取严格
//! 校验每个换行结尾的行，仅把最后一个换行之后「解析不出的非空尾部」当作断电撕
//! 裂写入丢弃。
//!
//! 本 crate 只接受 `Path`，不依赖配置类型。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use yaa_msg::Record;

/// 一个会话：JSONL 文件及其父目录（会话根）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub file: PathBuf,
    pub root: PathBuf,
}

impl Session {
    /// 以 JSONL 文件路径构造，会话根取该文件的父目录。
    pub fn new(file: impl Into<PathBuf>) -> Self {
        let file = file.into();
        let root = file.parent().map(Path::to_path_buf).unwrap_or_default();
        Self { file, root }
    }
}

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误：{0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "会话文件第 {line} 行无法解析：{source}（可能是旧版 schema，可运行 `yaa --clear` 归档旧会话后重开）"
    )]
    Corrupt {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("会话布局非法：{0}")]
    Layout(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// 加载整个会话。文件不存在视为空。
///
/// 换行结束的行严格解析，失败即报中部损坏；末尾没有换行结尾的一段，能解析则保留，
/// 不能解析（断电撕裂）则丢弃。
pub fn load(session: &Session) -> Result<Vec<Record>> {
    let data = match fs::read(&session.file) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    parse_lines(&data, true)
}

/// 从 `offset` 增量读取到文件末尾，返回记录与新的偏移。
///
/// 新偏移始终指向最后一个完整换行之后，尚未写完的半行会在下次被重新读取。
pub fn read_from(session: &Session, offset: u64) -> Result<(Vec<Record>, u64)> {
    let mut file = File::open(&session.file)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    // 末尾没有换行时，若整段就是一条可解析的完整记录（例如落到一半且尚未补换行），
    // 则与 `load` 保持一致地消费它并把偏移推进到文件末尾。
    let end = match data.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => {
            if !data.is_empty()
                && let Ok(record) = serde_json::from_slice::<Record>(&data)
            {
                return Ok((vec![record], offset + data.len() as u64));
            }
            return Ok((Vec::new(), offset));
        }
    };
    let mut records = parse_lines(&data[..end], false)?;
    let trailing = &data[end..];
    if !trailing.is_empty()
        && let Ok(record) = serde_json::from_slice::<Record>(trailing)
    {
        records.push(record);
        return Ok((records, offset + data.len() as u64));
    }
    Ok((records, offset + end as u64))
}

/// 原子追加一批记录：整批序列化到一个缓冲区，单次 `write_all` 后 `sync_all`。
///
/// 追加前先修复文件末尾：若最后一个换行之后的尾部能解析为完整记录，就补一个换行
/// 把它封口；若不能解析（断电撕裂的半行），就截断丢弃。这样新批次不会与残段粘连
/// 而产生中部解析错误。
pub fn append(session: &Session, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut batch = Vec::new();
    for record in records {
        serde_json::to_writer(&mut batch, record)?;
        batch.push(b'\n');
    }
    // 目录创建放在真正写入处：会话解析本身只读，`--status`/`--clear` 不该物化空目录。
    if let Some(parent) = session.file.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&session.file)?;
    repair_trailing(&mut file)?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&batch)?;
    file.sync_all()?;
    Ok(())
}

/// 修复文件末尾的残段，使下一次追加从干净的换行边界开始。
///
/// 常见路径（文件以换行结尾，或为空）只读末尾一个字节，避免每次追加都整读文件；
/// 只有确实存在无换行尾部时才回退到整读以定位残段。
fn repair_trailing(file: &mut File) -> Result<()> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    // 最后一个换行之后即残段起点；整份文件都没有换行时起点为 0。
    let start = data
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    if serde_json::from_slice::<Record>(&data[start..]).is_ok() {
        // 完整但未落尾换行的末条记录：补换行封口，保留内容。
        file.seek(SeekFrom::End(0))?;
        file.write_all(b"\n")?;
    } else {
        // 断电撕裂的半行：截断丢弃，避免与新批次粘连。
        file.set_len(start as u64)?;
    }
    Ok(())
}

/// 归档会话：把 `default.jsonl` 移到 `<history_root>/<sid>/sessions-<ts>.jsonl`。
///
/// `<sid>` 取会话根目录名；会话根没有目录名（会话文件就是裸文件名、无父目录）时，
/// 回退为 `sha256(绝对文件路径)` 前 32 位十六进制，保证 `--clear --session s.jsonl`
/// 也能归档而不是报错。`history_root` 由调用方给出，避免猜测数据根。只移动 JSONL，
/// 不动 `outputs/`，因此归档记录里的相对路径仍然有效。
pub fn archive(session: &Session, history_root: &Path, ts: i64) -> Result<PathBuf> {
    let sid = match session.root.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path_sid(&session.file),
    };

    let target_dir = history_root.join(&sid);
    fs::create_dir_all(&target_dir)?;
    let target = unique_archive_path(&target_dir, ts);
    fs::rename(&session.file, &target)?;
    Ok(target)
}

/// 归档目标路径：`sessions-<ts>.jsonl` 已存在时追加 `-<n>`（n 从 1 递增），
/// 不再静默覆盖同一毫秒内的既有归档。
fn unique_archive_path(target_dir: &Path, ts: i64) -> PathBuf {
    let target = target_dir.join(format!("sessions-{ts}.jsonl"));
    if !target.exists() {
        return target;
    }
    for n in 1u32.. {
        let candidate = target_dir.join(format!("sessions-{ts}-{n}.jsonl"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 序号耗尽不应发生")
}

/// 裸文件名会话的 sid 兜底：绝对路径的 sha256 前 32 位十六进制。
fn path_sid(file: &Path) -> String {
    let absolute = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(absolute.as_os_str().as_encoded_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..32].to_string()
}

/// 解析以换行分隔的记录。
///
/// 换行结束的行一律严格解析，失败即视为文件中部损坏。文件末尾若有一段没有换行
/// 结尾的内容：`keep_incomplete_trailing` 为真时尝试解析，成功则保留（完整但未
/// 落尾换行的末条记录），失败则丢弃（断电撕裂）；为假时（增量读取）一律丢弃，
/// 交由下次读取重试。
fn parse_lines(data: &[u8], keep_incomplete_trailing: bool) -> Result<Vec<Record>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut start = 0usize;
    let mut line_no = 0usize;
    for (idx, byte) in data.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        line_no += 1;
        let line = &data[start..idx];
        start = idx + 1;
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_slice(line).map_err(|source| StoreError::Corrupt {
            line: line_no,
            source,
        })?;
        records.push(record);
    }

    let trailing = &data[start..];
    if keep_incomplete_trailing
        && !trailing.is_empty()
        && let Ok(record) = serde_json::from_slice::<Record>(trailing)
    {
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaa_msg::Record;

    fn session_in(dir: &Path) -> Session {
        Session::new(dir.join("default.jsonl"))
    }

    #[test]
    fn append_then_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let records = vec![Record::user("你好"), Record::ai("m", "h", "回复", None)];
        append(&session, &records).unwrap();

        let loaded = load(&session).unwrap();
        assert_eq!(loaded, records);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        assert!(load(&session).unwrap().is_empty());
    }

    #[test]
    fn load_tolerates_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let good = Record::user("good");
        append(&session, std::slice::from_ref(&good)).unwrap();

        // 模拟断电：在完整记录后追加半行。
        let mut file = OpenOptions::new().append(true).open(&session.file).unwrap();
        file.write_all(br#"{"type":"user","id":"torn"#).unwrap();
        file.sync_all().unwrap();

        assert_eq!(load(&session).unwrap(), vec![good]);
    }

    #[test]
    fn load_rejects_mid_file_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let mut data = serde_json::to_string(&Record::user("a")).unwrap();
        data.push('\n');
        data.push_str("not json\n");
        data.push_str(&serde_json::to_string(&Record::user("b")).unwrap());
        data.push('\n');
        fs::write(&session.file, data).unwrap();

        let err = load(&session).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }));
        let message = err.to_string();
        assert!(
            message.contains("可能是旧版 schema"),
            "错误应提示旧版 schema，实际：{message}"
        );
        assert!(
            message.contains("--clear"),
            "错误应提示 --clear，实际：{message}"
        );
    }

    #[test]
    fn load_rejects_unparseable_mid_file_line_with_clear_hint() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let mut data = serde_json::to_string(&Record::user("a")).unwrap();
        data.push('\n');
        data.push_str("{\"type\":\"tool_call\",\"hook\":{}}\n");
        data.push_str(&serde_json::to_string(&Record::user("b")).unwrap());
        data.push('\n');
        fs::write(&session.file, data).unwrap();

        let err = load(&session).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { line: 2, .. }));
        assert!(err.to_string().contains("--clear"));
    }

    #[test]
    fn read_from_offset_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = Record::user("one");
        append(&session, std::slice::from_ref(&first)).unwrap();
        let (records, offset) = read_from(&session, 0).unwrap();
        assert_eq!(records, vec![first]);
        assert_eq!(offset, fs::metadata(&session.file).unwrap().len());

        let (empty, same) = read_from(&session, offset).unwrap();
        assert!(empty.is_empty());
        assert_eq!(same, offset);

        let second = Record::ai("m", "h", "two", None);
        append(&session, std::slice::from_ref(&second)).unwrap();
        let (records, next) = read_from(&session, offset).unwrap();
        assert_eq!(records, vec![second]);
        assert!(next > offset);
    }

    #[test]
    fn read_from_partial_line_does_not_advance() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        fs::write(&session.file, b"{\"type\":\"user\"").unwrap();

        let (records, offset) = read_from(&session, 0).unwrap();
        assert!(records.is_empty());
        assert_eq!(offset, 0);
    }

    #[test]
    fn archive_moves_file_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sid_dir = dir.path().join("sessions").join("abc");
        fs::create_dir_all(&sid_dir).unwrap();
        let session = Session::new(sid_dir.join("default.jsonl"));
        let record = Record::user("归档");
        append(&session, std::slice::from_ref(&record)).unwrap();

        let history = dir.path().join("history");
        let target = archive(&session, &history, 1234).unwrap();
        assert!(!session.file.exists());
        assert!(target.exists());
        assert_eq!(target, history.join("abc").join("sessions-1234.jsonl"));
        assert_eq!(load(&Session::new(target)).unwrap(), vec![record]);
    }

    #[test]
    fn archive_twice_same_ts_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let sid_dir = dir.path().join("sessions").join("abc");
        fs::create_dir_all(&sid_dir).unwrap();
        let history = dir.path().join("history");

        let first_session = Session::new(sid_dir.join("default.jsonl"));
        let first_record = Record::user("第一次");
        append(&first_session, std::slice::from_ref(&first_record)).unwrap();
        let first = archive(&first_session, &history, 1234).unwrap();
        assert_eq!(first, history.join("abc").join("sessions-1234.jsonl"));

        let second_session = Session::new(sid_dir.join("default.jsonl"));
        let second_record = Record::user("第二次");
        append(&second_session, std::slice::from_ref(&second_record)).unwrap();
        let second = archive(&second_session, &history, 1234).unwrap();
        assert_eq!(second, history.join("abc").join("sessions-1234-1.jsonl"));

        assert!(first.exists(), "首次归档不应被覆盖");
        assert!(second.exists(), "同 ts 的第二次归档应追加序号");
        let first_loaded = load(&Session::new(first)).unwrap();
        let second_loaded = load(&Session::new(second)).unwrap();
        assert_eq!(first_loaded.len(), 1);
        assert_eq!(second_loaded.len(), 1);
        assert_eq!(first_loaded[0], first_record, "首次归档内容应保持独立");
        assert_eq!(second_loaded[0], second_record, "第二次归档内容应保持独立");
    }

    #[test]
    fn archive_bare_filename_falls_back_to_path_sid() {
        let dir = tempfile::tempdir().unwrap();
        // 会话文件是数据根之外的普通文件，会话根为空（无目录名）——模拟
        // `--clear --session s.jsonl` 的裸文件名形态。
        let file = dir.path().join("s.jsonl");
        let session = Session {
            file: file.clone(),
            root: PathBuf::new(),
        };
        let record = Record::user("裸文件名");
        append(&session, std::slice::from_ref(&record)).unwrap();

        let history = dir.path().join("history");
        let target = archive(&session, &history, 1234).unwrap();
        assert!(!file.exists(), "裸文件名会话应已归档");
        let sid = path_sid(&file);
        assert_eq!(sid.len(), 32);
        assert_eq!(target, history.join(&sid).join("sessions-1234.jsonl"));
        assert_eq!(load(&Session::new(target)).unwrap(), vec![record]);
    }

    #[test]
    fn load_keeps_complete_trailing_record_without_newline() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = Record::user("一");
        let second = Record::user("二");
        let mut data = serde_json::to_string(&first).unwrap();
        data.push('\n');
        data.push_str(&serde_json::to_string(&second).unwrap()); // 无尾换行
        fs::write(&session.file, data).unwrap();

        assert_eq!(load(&session).unwrap(), vec![first, second]);
    }

    #[test]
    fn read_from_does_not_consume_trailing_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let record = Record::user("完整");
        let mut data = serde_json::to_string(&record).unwrap();
        data.push('\n');
        fs::write(&session.file, data).unwrap();
        let len = fs::metadata(&session.file).unwrap().len();

        // 追加一段解析不出的撕裂残段：增量读取不应消费它。
        let mut file = OpenOptions::new().append(true).open(&session.file).unwrap();
        file.write_all(br#"{"type":"user","id":"torn"#).unwrap();
        file.sync_all().unwrap();

        let (records, offset) = read_from(&session, 0).unwrap();
        assert_eq!(records, vec![record]);
        assert_eq!(offset, len);
    }

    #[test]
    fn append_repairs_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let good = Record::user("good");
        append(&session, std::slice::from_ref(&good)).unwrap();

        // 模拟断电：完整记录后留下半行。
        let mut file = OpenOptions::new().append(true).open(&session.file).unwrap();
        file.write_all(br#"{"type":"user","id":"torn"#).unwrap();
        file.sync_all().unwrap();

        let new = Record::user("new");
        append(&session, std::slice::from_ref(&new)).unwrap();

        // 撕裂残段被丢弃，而不是与新记录粘连成非法 JSON。
        assert_eq!(load(&session).unwrap(), vec![good, new]);
    }

    #[test]
    fn append_preserves_complete_trailing_record_without_newline() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = Record::user("一");
        let second = Record::user("二");
        let mut data = serde_json::to_string(&first).unwrap();
        data.push('\n');
        data.push_str(&serde_json::to_string(&second).unwrap()); // 无尾换行
        fs::write(&session.file, data).unwrap();

        let third = Record::user("三");
        append(&session, std::slice::from_ref(&third)).unwrap();

        assert_eq!(load(&session).unwrap(), vec![first, second, third]);
    }

    #[test]
    fn read_from_consumes_parseable_trailing_record() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = Record::user("一");
        let second = Record::user("二");
        let mut data = serde_json::to_string(&first).unwrap();
        data.push('\n');
        data.push_str(&serde_json::to_string(&second).unwrap()); // 无尾换行
        fs::write(&session.file, data).unwrap();
        let len = fs::metadata(&session.file).unwrap().len();

        let (records, offset) = read_from(&session, 0).unwrap();
        assert_eq!(records, vec![first, second]);
        assert_eq!(offset, len);
    }

    #[test]
    fn empty_append_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[]).unwrap();
        assert!(!session.file.exists());
    }

    #[test]
    fn append_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(
            dir.path()
                .join("sessions")
                .join("sid")
                .join("default.jsonl"),
        );
        assert!(!session.root.exists());

        let record = Record::user("自动建目录");
        append(&session, std::slice::from_ref(&record)).unwrap();

        assert!(session.root.is_dir(), "append 应创建缺失的会话目录");
        assert_eq!(load(&session).unwrap(), vec![record]);
    }
}
