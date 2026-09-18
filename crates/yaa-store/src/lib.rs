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
    #[error("会话文件第 {line} 行损坏：{source}")]
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

    let end = match data.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => return Ok((Vec::new(), offset)),
    };
    let records = parse_lines(&data[..end], false)?;
    Ok((records, offset + end as u64))
}

/// 原子追加一批记录：整批序列化到一个缓冲区，单次 `write_all` 后 `sync_all`。
pub fn append(session: &Session, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut batch = Vec::new();
    for record in records {
        serde_json::to_writer(&mut batch, record)?;
        batch.push(b'\n');
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&session.file)?;
    file.write_all(&batch)?;
    file.sync_all()?;
    Ok(())
}

/// 归档会话：把 `default.jsonl` 移到 `<history_root>/<sid>/sessions-<ts>.jsonl`。
///
/// `<sid>` 取会话根目录名，`history_root` 由调用方给出，避免猜测数据根。只移动
/// JSONL，不动 `outputs/`，因此归档记录里的相对路径仍然有效。
pub fn archive(session: &Session, history_root: &Path, ts: i64) -> Result<PathBuf> {
    let sid = session
        .root
        .file_name()
        .ok_or_else(|| StoreError::Layout(format!("会话根无目录名：{}", session.root.display())))?;

    let target_dir = history_root.join(sid);
    fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(format!("sessions-{ts}.jsonl"));
    fs::rename(&session.file, &target)?;
    Ok(target)
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
        let records = vec![Record::user("你好"), Record::ai("m", "h", "回复")];
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

        assert!(matches!(
            load(&session).unwrap_err(),
            StoreError::Corrupt { .. }
        ));
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

        let second = Record::ai("m", "h", "two");
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

        // 追加一条尚未落尾换行的记录：增量读取不应消费它。
        let mut file = OpenOptions::new().append(true).open(&session.file).unwrap();
        file.write_all(
            serde_json::to_string(&Record::user("半条"))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.sync_all().unwrap();

        let (records, offset) = read_from(&session, 0).unwrap();
        assert_eq!(records, vec![record]);
        assert_eq!(offset, len);
    }

    #[test]
    fn empty_append_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[]).unwrap();
        assert!(!session.file.exists());
    }
}
