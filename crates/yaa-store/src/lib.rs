//! YAA 会话 SQLite 同步存储。
//!
//! 每个会话一个数据库（`session.db`）。事件表 append-only，是唯一事实源：每行
//! `payload` 保存完整 [`Record`] JSON，`type`/`ts` 由 SQLite 生成列派生。写入使用
//! `BEGIN IMMEDIATE` 短事务与 WAL + `synchronous=FULL`，断电只可能丢掉未提交的批次。
//!
//! 本 crate 只接受 `Path`，不依赖配置类型。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use yaa_msg::Record;

mod file_queue;
mod inbox;
mod spool;

pub use inbox::{
    InboxMessage, InboxPhase, cleanup_inbox_tmp, count_inbox, publish_inbox, remove_inbox,
    scan_inbox,
};
pub use spool::{
    SpoolMessage, cleanup_spool_tmp, count_spool, publish_spool, publish_spool_with_generation,
    remove_spool, scan_spool,
};

/// `application_id` 取 `"YAA1"` 的 ASCII 字节。
const APPLICATION_ID: i32 = 0x5941_4131;
/// 当前 schema 版本，写入 `PRAGMA user_version`。
const SCHEMA_VERSION: i32 = 1;
/// WAL-reset 修复所在的最低 SQLite 版本。
const MIN_SQLITE_VERSION: i32 = 3_051_003;
/// 读路径与归档的默认忙等待。
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5_000;
/// 追加的忙等待。
const APPEND_BUSY_TIMEOUT_MS: u64 = 5_000;
/// BUSY 重试的退避序列：最多 3 次额外尝试。
const RETRY_DELAYS_MS: [u64; 3] = [50, 100, 200];
/// 读路径撞见「文件非空但 schema 尚未提交」时的重试次数。
const INIT_WINDOW_READ_ATTEMPTS: usize = 8;
/// 每次读重试的等待时间。
const INIT_WINDOW_READ_DELAY: Duration = Duration::from_millis(15);
/// 最后在 `init.lock` 下复查的最长等待时间。
const INIT_WINDOW_LOCK_WAIT: Duration = Duration::from_millis(500);

/// 归档快照临时文件的“陈旧”阈值：超过此时长的 `.tmp.*` 视为崩溃残留并清理。
const STALE_ARCHIVE_TEMP_AGE: Duration = Duration::from_secs(3600);

// 编译期确认 bundled SQLite 足够新；运行期 `check_sqlite_version` 再校验一次。
const _: () = assert!(rusqlite::ffi::SQLITE_VERSION_NUMBER >= MIN_SQLITE_VERSION);

const SCHEMA_SQL: &str = r#"
CREATE TABLE session (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  sid         TEXT    NOT NULL,
  created_at  INTEGER NOT NULL,
  generation  INTEGER NOT NULL DEFAULT 1,
  app_version TEXT    NOT NULL,
  archived    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE events (
  seq     INTEGER PRIMARY KEY,
  id      TEXT    NOT NULL UNIQUE,
  payload TEXT    NOT NULL CHECK (json_valid(payload)),
  type    TEXT    GENERATED ALWAYS AS (json_extract(payload, '$.type')) VIRTUAL,
  ts      INTEGER GENERATED ALWAYS AS (json_extract(payload, '$.ts')) VIRTUAL,
  v       INTEGER NOT NULL DEFAULT 1,
  CHECK (id = json_extract(payload, '$.id'))
) STRICT;
"#;

/// 一个会话：数据库文件及其父目录（会话根）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub db: PathBuf,
    pub root: PathBuf,
    pub sid: String,
}

impl Session {
    /// 以数据库路径构造；会话根取父目录，`sid` 取父目录名。
    ///
    /// 裸文件名（无父目录名）时 `sid` 回退为绝对路径 sha256 的前 32 位十六进制。
    pub fn new(db: impl Into<PathBuf>) -> Self {
        let db = db.into();
        let root = db.parent().map(Path::to_path_buf).unwrap_or_default();
        let sid = match root.file_name() {
            Some(name) if !name.is_empty() => name.to_string_lossy().into_owned(),
            _ => path_sid(&db),
        };
        Self { db, root, sid }
    }

    /// 会话 artifact 文件名前缀：默认会话 `session.db` 为空，其余为 `<安全文件名>.`。
    ///
    /// 把显式 `--session` 的锁与文件 inbox 唯一化到各自会话，避免同一目录下的多个
    /// 显式会话共享 `runner.lock` / `inbox/`。前缀由**完整数据库文件名**派生（而非
    /// `file_stem`），因此 `a.db` 与 `a.sqlite` 不会碰撞；非 [`A-Za-z0-9._-`] 字符
    /// 替换为 `_`，超长名追加短 hash。前缀不额外存字段，因此 [`Session`] 的公开结构
    /// 与相等性保持不变。
    fn artifact_prefix(&self) -> String {
        match self.db.file_name() {
            Some(name) if name != std::ffi::OsStr::new("session.db") => {
                format!("{}.", sanitize_artifact_name(name))
            }
            _ => String::new(),
        }
    }

    /// 逻辑 runner 选举锁。
    pub fn runner_lock_path(&self) -> PathBuf {
        self.root
            .join(format!("{}runner.lock", self.artifact_prefix()))
    }

    /// clear EX / append SH 锁。
    pub fn clear_lock_path(&self) -> PathBuf {
        self.root
            .join(format!("{}clear.lock", self.artifact_prefix()))
    }

    /// 首次建库串行化锁。
    pub fn init_lock_path(&self) -> PathBuf {
        self.root
            .join(format!("{}init.lock", self.artifact_prefix()))
    }

    /// 文件 inbox 根目录（详见 [`crate::inbox`]）。
    pub fn inbox_dir(&self) -> PathBuf {
        self.root.join(format!("{}inbox", self.artifact_prefix()))
    }

    /// spool 根目录：发布失败的通知暂存于此，下次运行补投。
    pub fn spool_dir(&self) -> PathBuf {
        self.root.join(format!("{}spool", self.artifact_prefix()))
    }
}

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite 错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("不是 yaa 会话数据库：{}", path.display())]
    NotYaaSession { path: PathBuf },
    #[error("不支持的会话 schema 版本 {found}（当前支持 {supported}）")]
    UnsupportedSchema { found: i32, supported: i32 },
    #[error("重复的记录 id：{id}")]
    DuplicateId { id: String },
    #[error("会话记录 seq={seq} id={id} 无法解析：{source}")]
    Corrupt {
        seq: i64,
        id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("会话已归档，拒绝写入：{}", path.display())]
    Archived { path: PathBuf },
    #[error("SQLite 忙，操作 {operation} 重试耗尽")]
    Busy { operation: &'static str },
    #[error("会话布局非法：{0}")]
    Layout(String),
    #[error("inbox 错误：{0}")]
    Inbox(String),
    #[error("会话已归档：发布世代已变化（期望 {expected}，当前 {current}）")]
    GenerationChanged { expected: i64, current: i64 },
}

impl StoreError {
    /// 发布失败是否属于可重试类别（BUSY/IO），可降级写 spool。
    ///
    /// generation 变化、归档、载荷非法与 schema 错误都是终态，不得 spool。
    pub fn is_retryable(&self) -> bool {
        match self {
            StoreError::Busy { .. } | StoreError::Io(_) => true,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(inner, _)) => matches!(
                inner.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked | ErrorCode::SystemIoFailure
            ),
            _ => false,
        }
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// 加载整个会话。文件不存在或为 0 字节时视为空且不创建任何文件。
pub fn load(session: &Session) -> Result<Vec<Record>> {
    retry_init_window(session, || {
        let Some(conn) = open_existing(session, DEFAULT_BUSY_TIMEOUT_MS)? else {
            return Ok(Vec::new());
        };
        validate_yaa(&conn, &session.db)?;

        let mut stmt = conn.prepare("SELECT seq, id, payload FROM events ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (seq, id, payload) = row?;
            records.push(parse_payload(seq, id, &payload)?);
        }
        Ok(records)
    })
}

/// 读取 `seq > after_seq` 的记录，返回记录及其 `seq` 与新的水位。
///
/// 不存在时返回 `(vec![], 0)`；没有新记录时水位保持 `after_seq`。
pub fn read_after(session: &Session, after_seq: i64) -> Result<(Vec<(i64, Record)>, i64)> {
    let (rows, new_seq, _generation) = read_after_with_generation(session, after_seq)?;
    Ok((rows, new_seq))
}

/// 同一读快照内的会话水位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionWatermark {
    /// 归档代数；会话不存在时为 0。
    pub generation: i64,
    /// 当前最大 `seq`；空会话为 0。
    pub last_seq: i64,
}

/// [`read_after_with_generation`] 的返回值：`(rows, new_seq, generation)`。
pub type ReadAfterWithGeneration = (Vec<(i64, Record)>, i64, i64);

/// 原子读取会话水位（同一读快照）。
///
/// 会话不存在（或文件为 0 字节）时返回 `generation = 0, last_seq = 0`，不创建文件。
/// `generation` 与 `MAX(seq)` 在同一 `BEGIN` 读事务内读取，故不会与并发 clear 拼接出
/// 「跨 generation」的值。
pub fn watermark(session: &Session) -> Result<SessionWatermark> {
    retry_init_window(session, || {
        let Some(conn) = open_existing(session, DEFAULT_BUSY_TIMEOUT_MS)? else {
            return Ok(SessionWatermark {
                generation: 0,
                last_seq: 0,
            });
        };
        validate_yaa(&conn, &session.db)?;
        read_watermark(&conn)
    })
}

/// 读取 `seq > after_seq` 的记录，并返回读取时的 generation。
///
/// 返回 `(rows, new_seq, generation)`：`rows` 与 `generation` 来自同一个读快照，
/// 并发 `clear` 不会产生「旧 generation 的行 + 新 generation 值」的拼接结果。
/// 会话不存在时返回 `(vec![], 0, 0)`；没有新记录时 `new_seq` 保持 `after_seq`。
pub fn read_after_with_generation(
    session: &Session,
    after_seq: i64,
) -> Result<ReadAfterWithGeneration> {
    retry_init_window(session, || {
        let Some(conn) = open_existing(session, DEFAULT_BUSY_TIMEOUT_MS)? else {
            return Ok((Vec::new(), 0, 0));
        };
        validate_yaa(&conn, &session.db)?;

        let tx = conn.unchecked_transaction()?;
        let result = (|| {
            let generation = read_generation(&tx)?;
            let mut stmt =
                tx.prepare("SELECT seq, id, payload FROM events WHERE seq > ?1 ORDER BY seq")?;
            let rows = stmt.query_map([after_seq], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut records = Vec::new();
            let mut new_seq = after_seq;
            for row in rows {
                let (seq, id, payload) = row?;
                records.push((seq, parse_payload(seq, id, &payload)?));
                new_seq = seq;
            }
            Ok((records, new_seq, generation))
        })();
        tx.commit()?;
        result
    })
}

/// 当前最大 `seq`；空会话为 0。
pub fn last_seq(session: &Session) -> Result<i64> {
    Ok(watermark(session)?.last_seq)
}

/// 当前 generation；会话不存在为 0。
pub fn generation(session: &Session) -> Result<i64> {
    retry_init_window(session, || {
        let Some(conn) = open_existing(session, DEFAULT_BUSY_TIMEOUT_MS)? else {
            return Ok(0);
        };
        validate_yaa(&conn, &session.db)?;
        read_generation(&conn)
    })
}

/// 带 generation 守卫地发布一条 inbox 记录。
///
/// 内部持 `clear.lock` SH（与 `--clear` 的 EX 互斥），在临界区内读取当前 generation
/// 并校验，然后发布。`expected = None` 不校验；否则要求当前 generation 等于
/// `expected`，或 `expected == 0 && 当前 == 1`（允许首次建库）。校验失败时不发布。
pub fn publish_with_generation(
    session: &Session,
    phase: inbox::InboxPhase,
    record: &Record,
    expected: Option<i64>,
) -> Result<()> {
    ensure_root(session)?;
    let _clear = FileLock::shared(&session.clear_lock_path())?;
    let current = generation(session)?;
    if let Some(expected) = expected {
        let first_init = expected == 0 && current == 1;
        if current != expected && !first_init {
            return Err(StoreError::GenerationChanged { expected, current });
        }
    }
    inbox::publish_inbox(session, phase, record)
}

/// 严格追加一批记录。
///
/// 空批直接返回且不创建任何文件。重复 id 时整批回滚并报 [`StoreError::DuplicateId`]。
pub fn append(session: &Session, records: &[Record]) -> Result<()> {
    append_impl(session, records, false)
}

/// 幂等追加一批记录：已存在的 id 被忽略，不报错也不重复。
pub fn append_idempotent(session: &Session, records: &[Record]) -> Result<()> {
    append_impl(session, records, true)
}

fn append_impl(session: &Session, records: &[Record], idempotent: bool) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    ensure_root(session)?;
    with_busy_retry("append", || append_once(session, records, idempotent))
}

fn append_once(session: &Session, records: &[Record], idempotent: bool) -> Result<()> {
    let mut conn = open_for_append(session)?;
    validate_yaa(&conn, &session.db)?;
    if read_archived(&conn)? {
        return Err(StoreError::Archived {
            path: session.db.clone(),
        });
    }
    ensure_wal(&conn)?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let sql = if idempotent {
            "INSERT INTO events (id, payload) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING"
        } else {
            "INSERT INTO events (id, payload) VALUES (?1, ?2)"
        };
        let mut stmt = tx.prepare(sql)?;
        for record in records {
            let payload = serde_json::to_string(record)
                .map_err(|source| StoreError::Layout(format!("记录序列化失败：{source}")))?;
            match stmt.execute(params![record.id(), payload]) {
                Ok(_) => {}
                Err(err) if !idempotent && is_duplicate_id(&err) => {
                    return Err(StoreError::DuplicateId {
                        id: record.id().to_owned(),
                    });
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// 归档会话：快照写入 `history_root/<sid>/sessions-<ts>.db`，然后清空主库事件并递增 generation。
///
/// 调用方应已持有 runner.lock EX 与 clear.lock EX。任何一步失败都不会清空主库。
pub fn archive(session: &Session, history_root: &Path, ts: i64) -> Result<PathBuf> {
    check_sqlite_version()?;
    let mut conn = open_existing_required(session, DEFAULT_BUSY_TIMEOUT_MS)?;
    validate_yaa(&conn, &session.db)?;
    let count = count_events(&conn)?;

    let target_dir = history_root.join(&session.sid);
    fs::create_dir_all(&target_dir)?;
    cleanup_stale_archive_temps(&target_dir, STALE_ARCHIVE_TEMP_AGE);
    let final_path = unique_archive_path(&target_dir, ts)?;
    let tmp_path = temp_archive_path(&final_path);
    // VACUUM INTO 的目标必须不存在或为空；先清掉同名残留。
    match fs::remove_file(&tmp_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let result = archive_snapshot(&mut conn, &tmp_path, &final_path, &target_dir, count);
    if result.is_err() {
        // 尽力清理：临时文件，以及仍是 0 字节的空占位（rename 尚未发生）。
        // rename 成功后 final 必为完整快照（非 0 字节），不会被误删。
        let _ = fs::remove_file(&tmp_path);
        if is_zero_len(&final_path) {
            let _ = fs::remove_file(&final_path);
        }
    }
    result?;
    Ok(final_path)
}

fn archive_snapshot(
    conn: &mut Connection,
    tmp_path: &Path,
    final_path: &Path,
    target_dir: &Path,
    expected_count: i64,
) -> Result<()> {
    let tmp = tmp_path.to_str().ok_or_else(|| {
        StoreError::Layout(format!(
            "归档临时路径不是合法 UTF-8：{}",
            tmp_path.display()
        ))
    })?;
    conn.execute("VACUUM INTO ?1", params![tmp])?;

    verify_snapshot(tmp_path, expected_count)?;
    File::open(tmp_path)?.sync_all()?;
    fs::rename(tmp_path, final_path)?;
    File::open(target_dir)?.sync_all()?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM events", [])?;
    tx.execute(
        "UPDATE session SET generation = generation + 1, archived = 0 WHERE id = 1",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn verify_snapshot(path: &Path, expected_count: i64) -> Result<()> {
    let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::Layout(format!(
            "归档快照 integrity_check 失败：{integrity}"
        )));
    }
    validate_yaa(&conn, path)?;
    let count = count_events(&conn)?;
    if count != expected_count {
        return Err(StoreError::Layout(format!(
            "归档快照记录数不一致：期望 {expected_count}，实际 {count}"
        )));
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("UPDATE session SET archived = 1 WHERE id = 1", [])?;
    tx.commit()?;
    Ok(())
}

/// 文件级归档旧 JSONL：`rename` 到 `history_root/<sid>/sessions-<ts>.jsonl`。
///
/// `<sid>` 取 JSONL 父目录名；裸文件名（无父目录名）时回退为绝对路径 sha256 的
/// 前 32 位十六进制。同一毫秒内重复归档时追加 `-<n>` 序号，绝不覆盖既有历史。
pub fn archive_legacy_jsonl(jsonl: &Path, history_root: &Path, ts: i64) -> Result<PathBuf> {
    let sid = match jsonl.parent().and_then(Path::file_name) {
        Some(name) if !name.is_empty() => name.to_string_lossy().into_owned(),
        _ => path_sid(jsonl),
    };
    let target_dir = history_root.join(&sid);
    fs::create_dir_all(&target_dir)?;
    let target = unique_legacy_archive_path(&target_dir, ts)?;
    if let Err(err) = fs::rename(jsonl, &target) {
        // rename 失败时清掉 create_new 留下的空占位，避免残留误导。
        let _ = fs::remove_file(&target);
        return Err(err.into());
    }
    Ok(target)
}

/// 旧 JSONL 的归档目标：`sessions-<ts>.jsonl` 已存在时追加 `-<n>`。
fn unique_legacy_archive_path(target_dir: &Path, ts: i64) -> Result<PathBuf> {
    let target = target_dir.join(format!("sessions-{ts}.jsonl"));
    if let Some(path) = try_reserve(&target)? {
        return Ok(path);
    }
    for n in 1u32.. {
        let candidate = target_dir.join(format!("sessions-{ts}-{n}.jsonl"));
        if let Some(path) = try_reserve(&candidate)? {
            return Ok(path);
        }
    }
    unreachable!("u32 序号耗尽不应发生")
}

/// 标记归档库。归档库的 `append` 会被拒绝。
pub fn set_archived(session: &Session) -> Result<()> {
    let conn = open_existing_required(session, DEFAULT_BUSY_TIMEOUT_MS)?;
    validate_yaa(&conn, &session.db)?;
    let changed = conn.execute("UPDATE session SET archived = 1 WHERE id = 1", [])?;
    if changed == 0 {
        return Err(StoreError::Layout("会话缺少 session 行".into()));
    }
    Ok(())
}

/// 会话是否已归档。会话不存在时返回 `false`。
pub fn is_archived(session: &Session) -> Result<bool> {
    retry_init_window(session, || {
        let Some(conn) = open_existing(session, DEFAULT_BUSY_TIMEOUT_MS)? else {
            return Ok(false);
        };
        validate_yaa(&conn, &session.db)?;
        read_archived(&conn)
    })
}

/// 只读查询归档标志：不触发 init 窗口重试、不创建任何文件。
///
/// 供显式 `--to` 的严格校验使用：目标可能不是 yaa 库，必须在**不创建任何目标文件**
/// （含 `init.lock`/`-wal`/`-shm`）的前提下完成 `application_id`/`user_version`
/// 校验并读取 archived。数据库以 `immutable=1` 只读 URI 打开，SQLite 不会创建
/// 共享内存或 WAL 文件；文件不存在或为 0 字节时返回 `false`。
///
/// `immutable` 读的是最后一次 checkpoint 的主库内容；归档状态的最终裁决仍由发布
/// 路径在锁内复查，故不会把通知写进已归档会话。
pub fn archived_readonly(session: &Session) -> Result<bool> {
    let Some(conn) = open_existing_readonly(session)? else {
        return Ok(false);
    };
    validate_yaa(&conn, &session.db)?;
    read_archived(&conn)
}

/// 基于 `flock` 的文件锁。锁随 [`FileLock`] 的 `File` 关闭而释放。
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY：`file` 仍持有有效 fd；显式解锁后随 `File` 关闭再释放一次是无害的。
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl FileLock {
    /// 阻塞获取排他锁。
    pub fn exclusive(path: &Path) -> Result<FileLock> {
        Self::acquire(path, libc::LOCK_EX)
    }

    /// 非阻塞尝试排他锁；已被占用时返回 `None`。
    pub fn try_exclusive(path: &Path) -> Result<Option<FileLock>> {
        Self::try_acquire(path, libc::LOCK_EX)
    }

    /// 阻塞获取共享锁。
    pub fn shared(path: &Path) -> Result<FileLock> {
        Self::acquire(path, libc::LOCK_SH)
    }

    fn open(path: &Path) -> Result<File> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?)
    }

    fn acquire(path: &Path, operation: libc::c_int) -> Result<FileLock> {
        let file = Self::open(path)?;
        loop {
            // SAFETY：`file` 在调用期间保持打开，fd 有效。
            let rc = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if rc == 0 {
                return Ok(Self { file });
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
    }

    fn try_acquire(path: &Path, operation: libc::c_int) -> Result<Option<FileLock>> {
        let file = Self::open(path)?;
        loop {
            // SAFETY：`file` 在调用期间保持打开，fd 有效。
            let rc = unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(Self { file }));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err.into());
        }
    }
}

fn parse_payload(seq: i64, id: String, payload: &str) -> Result<Record> {
    serde_json::from_str(payload).map_err(|source| StoreError::Corrupt { seq, id, source })
}

fn check_sqlite_version() -> Result<()> {
    let found = rusqlite::version_number();
    if found < MIN_SQLITE_VERSION {
        return Err(StoreError::Layout(format!(
            "SQLite 版本过低：需要 ≥ 3.51.3，实际 {}",
            rusqlite::version()
        )));
    }
    Ok(())
}

fn open_existing(session: &Session, busy_timeout_ms: u64) -> Result<Option<Connection>> {
    check_sqlite_version()?;
    match fs::metadata(&session.db) {
        Ok(meta) if meta.len() > 0 => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let conn = Connection::open_with_flags(&session.db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|err| map_open_error(err, &session.db))?;
    apply_pragmas(&conn, busy_timeout_ms)?;
    Ok(Some(conn))
}

fn open_existing_required(session: &Session, busy_timeout_ms: u64) -> Result<Connection> {
    open_existing(session, busy_timeout_ms)?.ok_or_else(|| {
        StoreError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("会话数据库不存在：{}", session.db.display()),
        ))
    })
}

/// 以只读方式打开已存在的数据库；不存在或为 0 字节时返回 `None`。
///
/// 不创建数据库，也不走 [`retry_init_window`]（后者会创建 `init.lock`）。使用
/// `immutable=1` URI：SQLite 不获取锁、不创建 `-wal`/`-shm`，满足显式 `--to`
/// 「绝不创建目标文件」的约束。
fn open_existing_readonly(session: &Session) -> Result<Option<Connection>> {
    check_sqlite_version()?;
    match fs::metadata(&session.db) {
        Ok(meta) if meta.len() > 0 => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let uri = immutable_uri(&session.db);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|err| map_open_error(err, &session.db))?;
    conn.busy_timeout(Duration::from_millis(DEFAULT_BUSY_TIMEOUT_MS))?;
    Ok(Some(conn))
}

/// 构造 `file:<percent-encoded-path>?immutable=1` URI。
///
/// 只保留 URI 路径中的安全字符；其余字节（含空格、`?`、`#`、非 ASCII）逐字节
/// 百分号编码，避免路径中的特殊字符改变 URI 语义。
fn immutable_uri(path: &Path) -> String {
    use std::fmt::Write as _;

    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            // 写入 String 不会失败；忽略返回值以满足 `write!` 的 Result。
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri.push_str("?immutable=1");
    uri
}

/// 在读路径上容忍「数据库文件已非空、schema 尚未提交」的初始化窗口。
///
/// 数据库初始化先在 0 字节文件上写 SQLite 头，再在事务中提交 schema；两者之间打开
/// 并校验会得到 [`StoreError::NotYaaSession`]。此时绝不能把它当成普通非 yaa 库：
/// 先短暂重试，仍失败则尝试在 `init.lock` 下复查一次（初始化者持排他锁，等它完成）。
/// 使用 `try_exclusive` 而非阻塞 `exclusive`，避免调用方已持有该锁时自死锁。
fn retry_init_window<T>(session: &Session, mut read: impl FnMut() -> Result<T>) -> Result<T> {
    for _ in 0..INIT_WINDOW_READ_ATTEMPTS {
        match read() {
            Err(StoreError::NotYaaSession { .. }) if !is_missing_or_empty(&session.db)? => {
                thread::sleep(INIT_WINDOW_READ_DELAY);
            }
            other => return other,
        }
    }

    let deadline = Instant::now() + INIT_WINDOW_LOCK_WAIT;
    loop {
        if let Some(_init) = FileLock::try_exclusive(&session.init_lock_path())? {
            return read();
        }
        if Instant::now() >= deadline {
            return read();
        }
        thread::sleep(INIT_WINDOW_READ_DELAY);
    }
}

/// 打开（必要时初始化）用于追加的数据库。
///
/// 初始化窗口内数据库文件可能已非空但尚未提交 schema，此时校验会得到
/// `NotYaaSession`；在 `init.lock` 下等待并重试一次，避免把正在初始化的库误判为
/// 非 yaa 库。
fn open_for_append(session: &Session) -> Result<Connection> {
    ensure_initialized(session)?;
    let conn = open_append_connection(session)?;
    match validate_yaa(&conn, &session.db) {
        Ok(()) => Ok(conn),
        Err(StoreError::NotYaaSession { .. }) => {
            drop(conn);
            let _init = FileLock::exclusive(&session.init_lock_path())?;
            if is_missing_or_empty(&session.db)? {
                init_db(session)?;
            }
            let conn = open_append_connection(session)?;
            validate_yaa(&conn, &session.db)?;
            Ok(conn)
        }
        Err(err) => Err(err),
    }
}

/// `append` 用 `READ_WRITE | CREATE` 打开；首次建库已由初始化路径完成，
/// `CREATE` 只用于覆盖初始化后文件被外部删除的窄竞态。
fn open_append_connection(session: &Session) -> Result<Connection> {
    check_sqlite_version()?;
    let conn = Connection::open_with_flags(
        &session.db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|err| map_open_error(err, &session.db))?;
    apply_pragmas(&conn, APPEND_BUSY_TIMEOUT_MS)?;
    Ok(conn)
}

fn ensure_initialized(session: &Session) -> Result<()> {
    if !is_missing_or_empty(&session.db)? {
        return Ok(());
    }
    let _init = FileLock::exclusive(&session.init_lock_path())?;
    if is_missing_or_empty(&session.db)? {
        init_db(session)?;
    }
    Ok(())
}

fn init_db(session: &Session) -> Result<()> {
    check_sqlite_version()?;
    let mut conn = Connection::open(&session.db)?;
    apply_pragmas(&conn, DEFAULT_BUSY_TIMEOUT_MS)?;
    // 不在建表事务前切换 WAL：在 0 字节文件上执行 `PRAGMA journal_mode=WAL` 会立刻
    // 落盘主库一页；若随后初始化失败，会留下「非空但无 schema」的文件并被永久判为
    // 非 yaa 库。保持回滚日志模式建库（崩溃回滚为 0 字节、可重试），提交后由写入
    // 路径的 `ensure_wal` 负责切换。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA_SQL)?;
    tx.execute(
        "INSERT INTO session (id, sid, created_at, generation, app_version, archived)
         VALUES (1, ?1, ?2, 1, ?3, 0)",
        params![session.sid, yaa_msg::now_ms(), env!("CARGO_PKG_VERSION")],
    )?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.commit()?;
    Ok(())
}

fn apply_pragmas(conn: &Connection, busy_timeout_ms: u64) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
    conn.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA journal_size_limit = 67108864;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA cache_size = -16384;",
    )?;
    Ok(())
}

fn ensure_wal(conn: &Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Layout(format!(
            "无法启用 WAL，当前 journal_mode={mode}"
        )));
    }
    Ok(())
}

fn validate_yaa(conn: &Connection, path: &Path) -> Result<()> {
    let application_id = conn
        .query_row("PRAGMA application_id", [], |row| row.get::<_, i32>(0))
        .map_err(|err| map_open_error(err, path))?;
    if application_id != APPLICATION_ID {
        return Err(StoreError::NotYaaSession {
            path: path.to_path_buf(),
        });
    }

    let found = conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found < 1 {
        return Err(StoreError::NotYaaSession {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn map_open_error(err: rusqlite::Error, path: &Path) -> StoreError {
    match &err {
        rusqlite::Error::SqliteFailure(inner, _) if inner.code == ErrorCode::NotADatabase => {
            StoreError::NotYaaSession {
                path: path.to_path_buf(),
            }
        }
        _ => StoreError::Sqlite(err),
    }
}

fn read_archived(conn: &Connection) -> Result<bool> {
    let archived = conn
        .query_row("SELECT archived FROM session WHERE id = 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?
        .ok_or_else(|| StoreError::Layout("会话缺少 session 行".into()))?;
    Ok(archived != 0)
}

fn read_generation(conn: &Connection) -> Result<i64> {
    let generation = conn
        .query_row("SELECT generation FROM session WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| StoreError::Layout("会话缺少 session 行".into()))?;
    Ok(generation)
}

/// 在同一读事务内读取 generation 与 `MAX(seq)`。
fn read_watermark(conn: &Connection) -> Result<SessionWatermark> {
    let tx = conn.unchecked_transaction()?;
    let generation = read_generation(&tx)?;
    let last_seq = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
        row.get(0)
    })?;
    tx.commit()?;
    Ok(SessionWatermark {
        generation,
        last_seq,
    })
}

fn count_events(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
}

fn is_duplicate_id(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == ErrorCode::ConstraintViolation
                && matches!(
                    inner.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                )
    )
}

fn is_busy_error(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == ErrorCode::DatabaseBusy
    )
}

fn with_busy_retry<T>(
    operation: &'static str,
    mut attempt_fn: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut attempt = 0;
    loop {
        match attempt_fn() {
            Ok(value) => return Ok(value),
            Err(err) if is_busy_error(&err) => {
                if attempt >= RETRY_DELAYS_MS.len() {
                    return Err(StoreError::Busy { operation });
                }
                thread::sleep(Duration::from_millis(RETRY_DELAYS_MS[attempt]));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn ensure_root(session: &Session) -> Result<()> {
    if !session.root.as_os_str().is_empty() {
        fs::create_dir_all(&session.root)?;
    }
    Ok(())
}

fn is_missing_or_empty(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len() == 0),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err.into()),
    }
}

/// 归档目标路径：`sessions-<ts>.db` 已存在时追加 `-<n>`（n 从 1 递增）。
///
/// 用 `create_new` 原子占位，避免并发归档选中同一路径后互相覆盖。
fn unique_archive_path(target_dir: &Path, ts: i64) -> Result<PathBuf> {
    let target = target_dir.join(format!("sessions-{ts}.db"));
    if let Some(path) = try_reserve(&target)? {
        return Ok(path);
    }
    for n in 1u32.. {
        let candidate = target_dir.join(format!("sessions-{ts}-{n}.db"));
        if let Some(path) = try_reserve(&candidate)? {
            return Ok(path);
        }
    }
    unreachable!("u32 序号耗尽不应发生")
}

fn try_reserve(candidate: &Path) -> Result<Option<PathBuf>> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(candidate)
    {
        Ok(_) => Ok(Some(candidate.to_path_buf())),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn temp_archive_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    final_path.with_file_name(name)
}

/// 清理归档目录中陈旧的崩溃残留（临时快照、空占位），尽力而为。
///
/// 只处理 `sessions-` 前缀的文件：名字含 `.tmp.` 的临时快照，或 0 字节的
/// `sessions-*.db` / `sessions-*.jsonl` 空占位（失败的归档只剩占位；完整快照永远
/// 非 0 字节）。仅清理修改时间早于 `max_age` 的文件；任何错误静默忽略。
/// 活跃归档的写入是秒级，小时级阈值不会误删并发写者的文件。
fn cleanup_stale_archive_temps(target_dir: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(target_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("sessions-") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let is_temp = name.contains(".tmp.");
        let is_empty_placeholder =
            metadata.len() == 0 && (name.ends_with(".db") || name.ends_with(".jsonl"));
        if !is_temp && !is_empty_placeholder {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age >= max_age {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 文件长度是否为 0（不存在或读取失败均为 `false`）。
fn is_zero_len(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.len() == 0)
        .unwrap_or(false)
}

/// 裸文件名会话的 sid 兜底：绝对路径的 sha256 前 32 位十六进制。
fn path_sid(db: &Path) -> String {
    let absolute = std::path::absolute(db).unwrap_or_else(|_| db.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(absolute.as_os_str().as_encoded_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..32].to_string()
}

/// 把数据库文件名安全化为 artifact 前缀。
///
/// 只保留 `[A-Za-z0-9._-]`，其余字符替换为 `_`。非 UTF-8 文件名（lossy 转换会丢信息）
/// 或安全化后超过 64 字符时，截断到 64 字符并追加原始文件名字节 sha256 的前 8 位，
/// 避免不同文件名映射到同一前缀。
fn sanitize_artifact_name(name: &std::ffi::OsStr) -> String {
    let lossy = name.to_string_lossy();
    let mut replaced = false;
    let mut sanitized: String = lossy
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                replaced = true;
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push('_');
    }

    let truncated = sanitized.chars().count() > 64;
    if truncated {
        sanitized = sanitized.chars().take(64).collect();
    }
    // 只要发生过字符替换（如 `a b` → `a_b`），就必须追加原文件名的短 hash：
    // 否则 `a b.db` 与真正的 `a_b.db` 会共享锁与 inbox。超长与非 UTF-8 同理。
    if replaced || truncated || name.to_str().is_none() {
        let mut hasher = Sha256::new();
        hasher.update(name.as_encoded_bytes());
        let digest = format!("{:x}", hasher.finalize());
        sanitized.push_str(&digest[..8]);
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier, mpsc};

    use super::*;
    use yaa_msg::Record;

    fn session_in(dir: &Path) -> Session {
        Session::new(dir.join("session.db"))
    }

    /// 目录条目名（排序），用于断言只读路径没有新增文件。
    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn user(content: &str) -> Record {
        Record::user(content)
    }

    fn record_ts(record: &Record) -> i64 {
        match record {
            Record::User(r) => r.ts,
            Record::Ai(r) => r.ts,
            Record::ToolCall(r) => r.ts,
            Record::ToolResult(r) => r.ts,
            Record::System(r) => r.ts,
            Record::ApprovalPending(r) => r.ts,
            Record::ApprovalResolved(r) => r.ts,
        }
    }

    #[test]
    fn append_then_load_round_trip_and_seq_order() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let records = vec![
            user("一"),
            Record::ai("m", "h", "回复", None),
            Record::system("notify", "通知", Some("git status".into()), None),
        ];
        append(&session, &records).unwrap();

        assert_eq!(load(&session).unwrap(), records);

        let (rows, watermark) = read_after(&session, 0).unwrap();
        assert_eq!(
            rows.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            rows.into_iter()
                .map(|(_, record)| record)
                .collect::<Vec<_>>(),
            records
        );
        assert_eq!(watermark, 3);
        assert_eq!(last_seq(&session).unwrap(), 3);
        assert_eq!(generation(&session).unwrap(), 1);
        assert!(!is_archived(&session).unwrap());
    }

    #[test]
    fn schema_sets_application_id_user_version_and_generated_columns() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let record = user("生成列");
        append(&session, std::slice::from_ref(&record)).unwrap();

        let conn = Connection::open(&session.db).unwrap();
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        let user_version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let (kind, ts): (String, i64) = conn
            .query_row("SELECT type, ts FROM events WHERE seq = 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();

        assert_eq!(app_id, APPLICATION_ID);
        assert_eq!(user_version, SCHEMA_VERSION);
        assert_eq!(journal_mode, "wal");
        assert_eq!(kind, "user");
        assert_eq!(ts, record_ts(&record));
    }

    #[test]
    fn empty_append_is_noop_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("sid");
        let session = Session::new(root.join("session.db"));

        append(&session, &[]).unwrap();
        append_idempotent(&session, &[]).unwrap();

        assert!(!session.db.exists());
        assert!(!root.exists());
    }

    #[test]
    fn missing_session_reads_are_empty_and_do_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("sid");
        let session = Session::new(root.join("session.db"));

        assert!(load(&session).unwrap().is_empty());
        assert_eq!(read_after(&session, 0).unwrap(), (Vec::new(), 0));
        assert_eq!(read_after(&session, 42).unwrap(), (Vec::new(), 0));
        assert_eq!(last_seq(&session).unwrap(), 0);
        assert_eq!(generation(&session).unwrap(), 0);
        assert!(!is_archived(&session).unwrap());

        assert!(!session.db.exists());
        assert!(!root.exists());
    }

    #[test]
    fn zero_byte_file_is_empty_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        fs::write(&session.db, b"").unwrap();

        assert!(load(&session).unwrap().is_empty());
        assert_eq!(read_after(&session, 0).unwrap(), (Vec::new(), 0));
        assert_eq!(last_seq(&session).unwrap(), 0);
        assert_eq!(generation(&session).unwrap(), 0);
        assert!(!is_archived(&session).unwrap());
        assert_eq!(fs::metadata(&session.db).unwrap().len(), 0);
    }

    #[test]
    fn read_after_incremental_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let records = vec![user("一"), user("二"), user("三")];
        append(&session, &records).unwrap();

        let (all, watermark) = read_after(&session, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(watermark, 3);

        let (tail, watermark) = read_after(&session, 1).unwrap();
        assert_eq!(
            tail.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(tail[0].1, records[1]);
        assert_eq!(watermark, 3);

        let (none, watermark) = read_after(&session, 3).unwrap();
        assert!(none.is_empty());
        assert_eq!(watermark, 3);

        let (none, watermark) = read_after(&session, 10).unwrap();
        assert!(none.is_empty());
        assert_eq!(watermark, 10);
    }

    #[test]
    fn watermark_is_atomic_and_tracks_generation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        assert_eq!(
            watermark(&session).unwrap(),
            SessionWatermark {
                generation: 0,
                last_seq: 0
            }
        );

        let records = vec![user("一"), user("二"), user("三")];
        append(&session, &records).unwrap();
        assert_eq!(
            watermark(&session).unwrap(),
            SessionWatermark {
                generation: 1,
                last_seq: 3
            }
        );

        let history = dir.path().join("history");
        archive(&session, &history, 1).unwrap();
        assert_eq!(
            watermark(&session).unwrap(),
            SessionWatermark {
                generation: 2,
                last_seq: 0
            }
        );
    }

    #[test]
    fn read_after_with_generation_pairs_rows_with_snapshot_generation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pre = vec![user("前"), user("前二")];
        append(&session, &pre).unwrap();

        let (rows, new_seq, generation) = read_after_with_generation(&session, 0).unwrap();
        assert_eq!(
            rows.iter().map(|(_, record)| record).collect::<Vec<_>>(),
            pre.iter().collect::<Vec<_>>()
        );
        assert_eq!(new_seq, 2);
        assert_eq!(generation, 1);

        let history = dir.path().join("history");
        archive(&session, &history, 1).unwrap();
        let (rows, new_seq, generation) = read_after_with_generation(&session, 0).unwrap();
        assert!(rows.is_empty());
        assert_eq!(new_seq, 0);
        assert_eq!(generation, 2);

        let post = vec![user("后")];
        append(&session, &post).unwrap();
        let (rows, new_seq, generation) = read_after_with_generation(&session, 0).unwrap();
        assert_eq!(
            rows.iter().map(|(_, record)| record).collect::<Vec<_>>(),
            post.iter().collect::<Vec<_>>()
        );
        assert_eq!(new_seq, 1);
        assert_eq!(generation, 2);
    }

    #[test]
    fn read_after_with_generation_never_mixes_generations() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pre: Vec<_> = (0..4).map(|index| user(&format!("pre-{index}"))).collect();
        append(&session, &pre).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(2));
        let writer_session = session.clone();
        let writer_stop = Arc::clone(&stop);
        let writer_start = Arc::clone(&start);
        let writer = std::thread::spawn(move || {
            let conn = Connection::open(&writer_session.db).unwrap();
            conn.busy_timeout(Duration::from_secs(5)).unwrap();
            writer_start.wait();
            for index in 0..30 {
                conn.execute_batch(
                    "BEGIN IMMEDIATE;
                     DELETE FROM events;
                     UPDATE session SET generation = generation + 1 WHERE id = 1;
                     COMMIT;",
                )
                .unwrap();
                if index + 1 < 30 {
                    append(&writer_session, &[user(&format!("post-{index}"))]).unwrap();
                }
                // 给读线程留出观察新 generation 的窗口，避免它完全被写线程饿死。
                std::thread::sleep(Duration::from_millis(1));
            }
            writer_stop.store(true, Ordering::SeqCst);
        });

        start.wait();
        let mut saw_new_generation_rows = false;
        while !stop.load(Ordering::SeqCst) {
            let (rows, _new_seq, generation) = read_after_with_generation(&session, 0).unwrap();
            for (_, record) in rows {
                let Record::User(user) = &record else {
                    panic!("测试只写入 user 记录");
                };
                let id = record.id();
                let consistent = if generation == 1 {
                    user.content.starts_with("pre-")
                } else {
                    user.content.starts_with("post-")
                };
                assert!(
                    consistent,
                    "跨 generation 拼接：generation={generation} id={id} content={}",
                    user.content
                );
                if generation > 1 {
                    saw_new_generation_rows = true;
                }
            }
        }
        writer.join().unwrap();
        assert!(
            saw_new_generation_rows,
            "应至少观察到一次新 generation 的行"
        );
    }

    #[test]
    fn publish_with_generation_checks_expected_generation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[user("一")]).unwrap();

        publish_with_generation(&session, InboxPhase::Pending, &user("不校验"), None).unwrap();
        publish_with_generation(&session, InboxPhase::Pending, &user("匹配"), Some(1)).unwrap();
        let before = count_inbox(&session, InboxPhase::Pending).unwrap();

        let err = publish_with_generation(&session, InboxPhase::Pending, &user("过期"), Some(2))
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::GenerationChanged {
                    expected: 2,
                    current: 1
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            count_inbox(&session, InboxPhase::Pending).unwrap(),
            before,
            "校验失败不得发布"
        );

        let history = dir.path().join("history");
        archive(&session, &history, 1).unwrap();
        let err =
            publish_with_generation(&session, InboxPhase::Pending, &user("归档后过期"), Some(1))
                .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::GenerationChanged {
                    expected: 1,
                    current: 2
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            count_inbox(&session, InboxPhase::Pending).unwrap(),
            before,
            "归档后旧 generation 不得发布"
        );

        publish_with_generation(&session, InboxPhase::Pending, &user("新代"), Some(2)).unwrap();
        assert_eq!(
            count_inbox(&session, InboxPhase::Pending).unwrap(),
            before + 1
        );
    }

    #[test]
    fn captured_generation_archived_before_publish_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[user("既有")]).unwrap();
        let captured = watermark(&session).unwrap();
        assert_eq!(captured.generation, 1);

        // 模拟 `--clear`：在调用方捕获 generation 之后持 EX 归档。
        let clear = FileLock::exclusive(&session.clear_lock_path()).unwrap();
        let history = dir.path().join("history");
        archive(&session, &history, 1).unwrap();
        drop(clear);
        assert_eq!(generation(&session).unwrap(), 2);

        let err = publish_with_generation(
            &session,
            InboxPhase::Pending,
            &user("迟到"),
            Some(captured.generation),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::GenerationChanged {
                    expected: 1,
                    current: 2
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            count_inbox(&session, InboxPhase::Pending).unwrap(),
            0,
            "归档后不得发布过期通知"
        );
    }

    #[test]
    fn publish_with_generation_allows_first_init() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        // 会话尚不存在（generation 0）。
        publish_with_generation(&session, InboxPhase::Pending, &user("建库前"), Some(0)).unwrap();

        append(&session, &[user("建库")]).unwrap();
        assert_eq!(generation(&session).unwrap(), 1);
        publish_with_generation(&session, InboxPhase::Pending, &user("建库中"), Some(0)).unwrap();

        assert_eq!(count_inbox(&session, InboxPhase::Pending).unwrap(), 2);
    }

    #[test]
    fn publish_with_generation_waits_for_clear_exclusive_lock() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let held = FileLock::exclusive(&session.clear_lock_path()).unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let publisher = session.clone();
        let handle = std::thread::spawn(move || {
            publish_with_generation(&publisher, InboxPhase::Pending, &user("等待"), Some(0))
                .unwrap();
            done_tx.send(()).unwrap();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "clear EX 持有时发布应阻塞在 SH"
        );
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("clear EX 释放后发布应完成");
        handle.join().unwrap();
    }

    #[test]
    fn duplicate_id_strict_append_rolls_back_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = user("原始");
        append(&session, std::slice::from_ref(&first)).unwrap();

        let fresh = user("新记录");
        let duplicate = first.clone();
        let err = append(&session, &[fresh.clone(), duplicate]).unwrap_err();
        let StoreError::DuplicateId { id } = err else {
            panic!("应报 DuplicateId，实际：{err:?}");
        };
        assert_eq!(id, first.id());

        assert_eq!(load(&session).unwrap(), vec![first]);
        assert_eq!(last_seq(&session).unwrap(), 1);
    }

    #[test]
    fn idempotent_append_ignores_existing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let first = user("一");
        let second = user("二");
        append(&session, &[first.clone(), second.clone()]).unwrap();

        let third = user("三");
        append_idempotent(&session, &[second.clone(), third.clone()]).unwrap();
        append_idempotent(&session, &[first.clone(), second.clone(), third.clone()]).unwrap();

        assert_eq!(load(&session).unwrap(), vec![first, second, third]);
        assert_eq!(last_seq(&session).unwrap(), 3);
    }

    #[test]
    fn read_paths_tolerate_concurrent_initialization() {
        let dir = tempfile::tempdir().unwrap();
        for round in 0..50 {
            let root = dir.path().join(format!("round-{round}"));
            fs::create_dir_all(&root).unwrap();
            let session = session_in(&root);
            let start = Arc::new(Barrier::new(2));

            let writer_session = session.clone();
            let writer_start = Arc::clone(&start);
            let writer = thread::spawn(move || {
                writer_start.wait();
                append(&writer_session, &[user("并发")]).unwrap();
            });

            start.wait();
            // 读可能发生在建库前、建库中（schema 尚未提交）或建库后。
            // 前两者都必须被当作空/构建中，而不是「非 yaa 会话」。
            let loaded = load(&session).unwrap();
            assert!(loaded.len() <= 1, "第 {round} 轮读到了未来记录");
            let watermark = watermark(&session).unwrap();
            assert!(watermark.last_seq <= 1, "第 {round} 轮水位越界");
            let (_, _, generation) = read_after_with_generation(&session, 0).unwrap();
            assert!(generation <= 1, "第 {round} 轮 generation 越界");
            assert!(!is_archived(&session).unwrap());

            writer.join().unwrap();
            assert_eq!(load(&session).unwrap().len(), 1);
        }
    }

    #[test]
    fn non_yaa_database_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        {
            let conn = Connection::open(&session.db).unwrap();
            conn.execute_batch("CREATE TABLE other (x INTEGER);")
                .unwrap();
        }

        let err = load(&session).unwrap_err();
        assert!(matches!(err, StoreError::NotYaaSession { .. }), "{err:?}");
        let err = append(&session, &[user("不应写入")]).unwrap_err();
        assert!(matches!(err, StoreError::NotYaaSession { .. }), "{err:?}");

        let conn = Connection::open(&session.db).unwrap();
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'events'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(app_id, 0, "不得把普通 SQLite 库改写成 yaa 库");
        assert_eq!(events, 0);
    }

    /// F6：显式 `--to` 的只读校验对非 yaa 库（即使处于 WAL 模式）也不得创建
    /// `init.lock`/`-wal`/`-shm`。
    #[test]
    fn archived_readonly_rejects_non_yaa_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        {
            let conn = Connection::open(&session.db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch("CREATE TABLE other (x INTEGER);")
                .unwrap();
        }
        let before = dir_entries(dir.path());

        let err = archived_readonly(&session).unwrap_err();
        assert!(matches!(err, StoreError::NotYaaSession { .. }), "{err:?}");
        assert_eq!(
            dir_entries(dir.path()),
            before,
            "只读校验不得创建 init.lock/-wal/-shm"
        );
    }

    #[test]
    fn future_user_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[user("一")]).unwrap();
        {
            let conn = Connection::open(&session.db).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }

        let err = load(&session).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::UnsupportedSchema {
                    found: 2,
                    supported: 1
                }
            ),
            "{err:?}"
        );
        let err = append(&session, &[user("二")]).unwrap_err();
        assert!(
            matches!(err, StoreError::UnsupportedSchema { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn corrupt_payload_reports_seq_and_id() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let record = user("坏");
        append(&session, std::slice::from_ref(&record)).unwrap();
        {
            let conn = Connection::open(&session.db).unwrap();
            conn.execute_batch("UPDATE events SET payload = '{\"foo\":\"bar\"}' WHERE seq = 1;")
                .unwrap();
        }

        let err = load(&session).unwrap_err();
        let StoreError::Corrupt { seq, id, .. } = err else {
            panic!("应报 Corrupt");
        };
        assert_eq!(seq, 1);
        assert_eq!(id, record.id());
    }

    #[test]
    fn archive_snapshots_and_resets_main_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("abc");
        let session = Session::new(root.join("session.db"));
        let records = vec![user("一"), user("二"), user("三")];
        append(&session, &records).unwrap();

        let history = dir.path().join("history");
        let target = archive(&session, &history, 1234).unwrap();
        assert_eq!(target, history.join("abc").join("sessions-1234.db"));
        assert!(target.exists());

        let archived = Session::new(target.clone());
        assert_eq!(load(&archived).unwrap(), records);
        assert!(is_archived(&archived).unwrap());
        let err = append(&archived, &[user("拒绝")]).unwrap_err();
        assert!(matches!(err, StoreError::Archived { .. }), "{err:?}");

        assert!(load(&session).unwrap().is_empty());
        assert_eq!(last_seq(&session).unwrap(), 0);
        assert_eq!(generation(&session).unwrap(), 2);
        assert!(!is_archived(&session).unwrap());

        let conn = Connection::open(&target).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");

        let after = user("归档后");
        append(&session, std::slice::from_ref(&after)).unwrap();
        assert_eq!(load(&session).unwrap(), vec![after]);
        assert_eq!(generation(&session).unwrap(), 2);
    }

    #[test]
    fn set_archived_marks_database_and_rejects_append() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[user("一")]).unwrap();

        set_archived(&session).unwrap();
        assert!(is_archived(&session).unwrap());

        let err = append(&session, &[user("拒绝")]).unwrap_err();
        assert!(matches!(err, StoreError::Archived { .. }), "{err:?}");
        assert_eq!(load(&session).unwrap().len(), 1);
    }

    #[test]
    fn archive_same_ts_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("abc");
        let session = Session::new(root.join("session.db"));
        let history = dir.path().join("history");

        let first_record = user("第一次");
        append(&session, std::slice::from_ref(&first_record)).unwrap();
        let first = archive(&session, &history, 1234).unwrap();

        let second_record = user("第二次");
        append(&session, std::slice::from_ref(&second_record)).unwrap();
        let second = archive(&session, &history, 1234).unwrap();

        assert_eq!(first, history.join("abc").join("sessions-1234.db"));
        assert_eq!(second, history.join("abc").join("sessions-1234-1.db"));
        assert_eq!(load(&Session::new(first)).unwrap(), vec![first_record]);
        assert_eq!(load(&Session::new(second)).unwrap(), vec![second_record]);
    }

    #[test]
    fn archive_missing_session_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let err = archive(&session, &dir.path().join("history"), 1).unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "{err:?}");
        assert!(!session.db.exists());
    }

    #[test]
    fn concurrent_appends_from_multiple_sessions_do_not_lose_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions").join("sid").join("session.db");
        let threads = 6;
        let per_thread = 20;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let db = db.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let session = Session::new(db);
                    let records: Vec<_> = (0..per_thread)
                        .map(|index| user(&format!("{thread}-{index}")))
                        .collect();
                    barrier.wait();
                    append(&session, &records).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let session = Session::new(db);
        let loaded = load(&session).unwrap();
        assert_eq!(loaded.len(), threads * per_thread);
        assert_eq!(last_seq(&session).unwrap(), (threads * per_thread) as i64);

        let unique: HashSet<_> = loaded.iter().map(|record| record.id().to_owned()).collect();
        assert_eq!(unique.len(), loaded.len(), "并发追加不得产生重复记录");

        let (rows, _) = read_after(&session, 0).unwrap();
        let seqs: Vec<_> = rows.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(
            seqs,
            (1..=loaded.len() as i64).collect::<Vec<_>>(),
            "seq 必须连续且单调"
        );
    }

    #[test]
    fn append_waits_for_a_concurrent_immediate_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        append(&session, &[user("初始化")]).unwrap();

        let db = session.db.clone();
        let (locked_tx, locked_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(&db).unwrap();
            conn.busy_timeout(Duration::from_secs(5)).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            conn.execute_batch("COMMIT;").unwrap();
        });

        locked_rx.recv().unwrap();
        append(&session, &[user("等待后成功")]).unwrap();
        holder.join().unwrap();

        assert_eq!(load(&session).unwrap().len(), 2);
    }

    #[test]
    fn file_lock_try_exclusive_conflicts_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner.lock");

        let held = FileLock::try_exclusive(&path).unwrap().expect("首次应成功");
        assert!(
            FileLock::try_exclusive(&path).unwrap().is_none(),
            "已被占用时应返回 None"
        );
        drop(held);
        assert!(
            FileLock::try_exclusive(&path).unwrap().is_some(),
            "释放后应可再次获取"
        );
        assert!(path.exists());
    }

    #[test]
    fn shared_and_exclusive_locks_are_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clear.lock");

        let shared = FileLock::shared(&path).unwrap();
        assert!(FileLock::try_exclusive(&path).unwrap().is_none());
        drop(shared);
        let exclusive = FileLock::try_exclusive(&path)
            .unwrap()
            .expect("共享释放后应成功");

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let shared_path = path.clone();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _lock = FileLock::shared(&shared_path).unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "排他锁持有时共享锁应阻塞"
        );
        drop(exclusive);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("排他锁释放后共享锁应获取成功");
        waiter.join().unwrap();
    }

    #[test]
    fn lock_paths_live_under_session_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("sid");
        let session = Session::new(root.join("session.db"));

        assert_eq!(session.root, root);
        assert_eq!(session.sid, "sid");
        assert_eq!(session.runner_lock_path(), root.join("runner.lock"));
        assert_eq!(session.clear_lock_path(), root.join("clear.lock"));
        assert_eq!(session.init_lock_path(), root.join("init.lock"));
    }

    #[test]
    fn default_session_inbox_dir_has_no_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("sid");
        let session = Session::new(root.join("session.db"));

        assert_eq!(session.inbox_dir(), root.join("inbox"));
    }

    #[test]
    fn explicit_session_artifacts_are_unique_per_full_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = Session::new(dir.path().join("a.db"));
        let second = Session::new(dir.path().join("b.db"));

        assert_eq!(
            first.runner_lock_path(),
            dir.path().join("a.db.runner.lock")
        );
        assert_eq!(first.clear_lock_path(), dir.path().join("a.db.clear.lock"));
        assert_eq!(first.init_lock_path(), dir.path().join("a.db.init.lock"));
        assert_eq!(first.inbox_dir(), dir.path().join("a.db.inbox"));

        assert_eq!(
            second.runner_lock_path(),
            dir.path().join("b.db.runner.lock")
        );
        assert_eq!(second.clear_lock_path(), dir.path().join("b.db.clear.lock"));
        assert_eq!(second.init_lock_path(), dir.path().join("b.db.init.lock"));
        assert_eq!(second.inbox_dir(), dir.path().join("b.db.inbox"));

        assert_ne!(first.runner_lock_path(), second.runner_lock_path());
        assert_ne!(first.clear_lock_path(), second.clear_lock_path());
        assert_ne!(first.init_lock_path(), second.init_lock_path());
        assert_ne!(first.inbox_dir(), second.inbox_dir());
    }

    #[test]
    fn explicit_session_names_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a_db = Session::new(dir.path().join("a.db"));
        let a_sqlite = Session::new(dir.path().join("a.sqlite"));
        let upper = Session::new(dir.path().join("A.db"));

        let sessions = [a_db, a_sqlite, upper];
        let path_sets = vec![
            sessions
                .iter()
                .map(Session::runner_lock_path)
                .collect::<Vec<_>>(),
            sessions
                .iter()
                .map(Session::clear_lock_path)
                .collect::<Vec<_>>(),
            sessions
                .iter()
                .map(Session::init_lock_path)
                .collect::<Vec<_>>(),
            sessions.iter().map(Session::inbox_dir).collect::<Vec<_>>(),
        ];
        for paths in path_sets {
            let unique: HashSet<_> = paths.iter().collect();
            assert_eq!(
                unique.len(),
                3,
                "不同会话名不得共享 artifact 路径：{paths:?}"
            );
        }
    }

    #[test]
    fn sanitized_artifact_names_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = Session::new(dir.path().join("a b.db"));
        let underscored = Session::new(dir.path().join("a_b.db"));

        assert_ne!(spaced.runner_lock_path(), underscored.runner_lock_path());
        assert_ne!(spaced.clear_lock_path(), underscored.clear_lock_path());
        assert_ne!(spaced.init_lock_path(), underscored.init_lock_path());
        assert_ne!(spaced.inbox_dir(), underscored.inbox_dir());
        // 发生替换的名字必须带短 hash，不能与「本来就是下划线」的名字相同。
        assert!(
            spaced
                .runner_lock_path()
                .file_name()
                .is_some_and(|name| name.len() > "a_b.db.runner.lock".len()),
            "替换后的 artifact 名应追加短 hash：{:?}",
            spaced.runner_lock_path()
        );
    }

    #[test]
    fn non_utf8_session_names_are_distinct() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let first = Session::new(dir.path().join(OsString::from_vec(b"a\xff.db".to_vec())));
        let second = Session::new(dir.path().join(OsString::from_vec(b"a\xfe.db".to_vec())));

        assert_ne!(first.runner_lock_path(), second.runner_lock_path());
        assert_ne!(first.inbox_dir(), second.inbox_dir());
    }

    #[test]
    fn explicit_sessions_hold_independent_runner_locks() {
        let dir = tempfile::tempdir().unwrap();
        let first = Session::new(dir.path().join("a.db"));
        let second = Session::new(dir.path().join("b.db"));

        let held = FileLock::try_exclusive(&first.runner_lock_path())
            .unwrap()
            .expect("首个会话应能取得自己的 runner 锁");
        assert!(
            FileLock::try_exclusive(&second.runner_lock_path())
                .unwrap()
                .is_some(),
            "不同显式会话的 runner 锁不得互相阻塞"
        );
        drop(held);
    }

    #[test]
    fn bare_filename_sid_uses_absolute_path_hash() {
        let db = PathBuf::from("bare.db");
        let session = Session::new(db.clone());

        assert_eq!(session.root, PathBuf::new());
        assert_eq!(session.sid, path_sid(&db));
        assert_eq!(session.sid.len(), 32);
        assert!(session.sid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn append_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("sid");
        let session = Session::new(root.join("session.db"));
        assert!(!root.exists());

        let record = user("自动建目录");
        append(&session, std::slice::from_ref(&record)).unwrap();

        assert!(root.is_dir());
        assert_eq!(load(&session).unwrap(), vec![record]);
    }

    #[test]
    fn archive_legacy_jsonl_moves_file_under_parent_sid() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("abc");
        fs::create_dir_all(&root).unwrap();
        let jsonl = root.join("default.jsonl");
        fs::write(&jsonl, b"{\"type\":\"user\"}\n").unwrap();

        let history = dir.path().join("history");
        let target = archive_legacy_jsonl(&jsonl, &history, 1234).unwrap();

        assert_eq!(target, history.join("abc").join("sessions-1234.jsonl"));
        assert!(!jsonl.exists(), "原 JSONL 应已移走");
        assert_eq!(fs::read(&target).unwrap(), b"{\"type\":\"user\"}\n");
    }

    #[test]
    fn archive_legacy_jsonl_same_ts_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("abc");
        fs::create_dir_all(&root).unwrap();
        let history = dir.path().join("history");

        let first_jsonl = root.join("first.jsonl");
        fs::write(&first_jsonl, b"first\n").unwrap();
        let first = archive_legacy_jsonl(&first_jsonl, &history, 1234).unwrap();

        let second_jsonl = root.join("second.jsonl");
        fs::write(&second_jsonl, b"second\n").unwrap();
        let second = archive_legacy_jsonl(&second_jsonl, &history, 1234).unwrap();

        assert_eq!(first, history.join("abc").join("sessions-1234.jsonl"));
        assert_eq!(second, history.join("abc").join("sessions-1234-1.jsonl"));
        assert_eq!(fs::read(&first).unwrap(), b"first\n");
        assert_eq!(fs::read(&second).unwrap(), b"second\n");
    }

    #[test]
    fn archive_cleanup_removes_only_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("sessions-1.db.tmp.123");
        let empty_db = dir.path().join("sessions-2.db");
        let empty_jsonl = dir.path().join("sessions-2.jsonl");
        let kept = dir.path().join("sessions-1.db");
        let other_tmp = dir.path().join("user-file.tmp.data");
        let other = dir.path().join("notes.txt");
        fs::write(&stale, b"x").unwrap();
        fs::write(&empty_db, b"").unwrap();
        fs::write(&empty_jsonl, b"").unwrap();
        fs::write(&kept, b"y").unwrap();
        fs::write(&other_tmp, b"z").unwrap();
        fs::write(&other, b"w").unwrap();

        // max_age 为零：所有符合模式的残留都按“陈旧”处理。
        cleanup_stale_archive_temps(dir.path(), Duration::ZERO);

        assert!(!stale.exists(), "陈旧临时快照应被清理");
        assert!(!empty_db.exists(), "0 字节归档占位应被清理");
        assert!(!empty_jsonl.exists(), "0 字节旧格式占位应被清理");
        assert!(kept.exists(), "正式归档文件不得删除");
        assert!(other_tmp.exists(), "非 sessions- 前缀的文件不得删除");
        assert!(other.exists(), "无关文件不得删除");
    }
}
