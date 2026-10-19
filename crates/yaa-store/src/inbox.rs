//! 文件 inbox：多客户端向单 runner 注入消息的无锁投递箱。
//!
//! 会话根下 `<prefix>inbox/` 含四个子目录：
//! - `tmp/`：发布中间文件；崩溃残留由 [`cleanup_inbox_tmp`] 清理，不参与扫描。
//! - `pending/`：普通 user 与外部 notify。
//! - `init/`：init 钩子期间的 notify。
//! - `rejected/`：解析失败或非法载荷。
//!
//! 发布 = 写 `tmp/<唯一名>` → `sync_all` → `rename` 到目标 phase 目录 → fsync 目录。
//! 同一文件系统内 `rename` 原子，故无锁即可多进程安全；消费后直接删除文件，崩溃在
//! “已 append、未删除”之间时文件会重现，由调用方以 `append_idempotent` 按 id 去重。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use yaa_msg::Record;

use crate::{Result, Session, StoreError};

/// inbox 阶段目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxPhase {
    /// 普通注入：user 与外部 notify。
    Pending,
    /// init 钩子期间的 notify。
    Init,
}

impl InboxPhase {
    /// 对应的子目录名。
    pub fn dir_name(self) -> &'static str {
        match self {
            InboxPhase::Pending => "pending",
            InboxPhase::Init => "init",
        }
    }
}

/// 扫描出的一条 inbox 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    /// 消息文件路径。
    pub path: PathBuf,
    /// 所在阶段。
    pub phase: InboxPhase,
    /// 解析出的记录。
    pub record: Record,
}

impl InboxMessage {
    /// 消息 id（等于文件名去扩展名）。
    pub fn id(&self) -> &str {
        self.record.id()
    }
}

const TMP_DIR: &str = "tmp";
const REJECTED_DIR: &str = "rejected";
const EXTENSION: &str = "json";
/// 单个 inbox 文件的大小上限：超限视为非法载荷。
const MAX_INBOX_BYTES: u64 = 1024 * 1024;
/// 消息 id 允许的最大字节数。
const MAX_ID_BYTES: usize = 128;

/// 进程内临时文件序号：即便并发发布同一条记录也不撞名。
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 发布一条记录到指定阶段。
///
/// 先把序列化载荷原子地落到 `tmp/`，再 `rename` 到 `<phase>/<id>.json`；任何一步失败
/// 都会清理临时文件后返回错误。多进程无锁并发安全。
///
/// `record.id()` 必须匹配 `[A-Za-z0-9_-]{1,128}`，否则直接报 [`StoreError::Inbox`]，
/// 不拼接任何路径，避免 id 逃逸出 phase 目录。
pub fn publish_inbox(session: &Session, phase: InboxPhase, record: &Record) -> Result<()> {
    if !is_valid_id(record.id()) {
        return Err(StoreError::Inbox(format!(
            "非法 inbox 消息 id：{:?}",
            record.id()
        )));
    }

    let inbox = session.inbox_dir();
    let tmp_dir = inbox.join(TMP_DIR);
    let phase_dir = inbox.join(phase.dir_name());
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(&phase_dir)?;

    let payload = serde_json::to_vec(record)
        .map_err(|err| StoreError::Inbox(format!("记录序列化失败：{err}")))?;

    let tmp_path = tmp_dir.join(tmp_file_name(record.id()));
    write_synced(&tmp_path, &payload)?;
    let target = phase_dir.join(format!("{}.{EXTENSION}", record.id()));
    if let Err(err) = fs::rename(&tmp_path, &target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    fsync_dir(&phase_dir)?;
    Ok(())
}

/// 扫描指定阶段的可消费消息，按文件名（ULID）升序返回。
///
/// 载荷必须是 `Record::User` 或 `Record::System { source == "notify" }`，且文件名
/// （去 `.json` 扩展名）必须等于 `record.id()`。解析失败或任一校验不符的文件会被尽力
/// 移动到 `rejected/` 并跳过——不返回、不报错。`tmp/` 不参与扫描。目录不存在时返回空。
pub fn scan_inbox(session: &Session, phase: InboxPhase) -> Result<Vec<InboxMessage>> {
    let inbox = session.inbox_dir();
    let phase_dir = inbox.join(phase.dir_name());
    let entries = match fs::read_dir(&phase_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut messages = Vec::new();
    let mut rejected_ready = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let oversized = entry
            .metadata()
            .map(|metadata| metadata.len() > MAX_INBOX_BYTES)
            .unwrap_or(false);
        if oversized {
            reject(&inbox, &path, &mut rejected_ready);
            continue;
        }
        let Some(record) = parse_candidate(&path) else {
            reject(&inbox, &path, &mut rejected_ready);
            continue;
        };
        if !is_acceptable_payload(&record)
            || !is_valid_id(record.id())
            || record.id() != file_stem(&path)
        {
            reject(&inbox, &path, &mut rejected_ready);
            continue;
        }
        messages.push(InboxMessage {
            path,
            phase,
            record,
        });
    }
    messages.sort_by(|left, right| left.path.file_name().cmp(&right.path.file_name()));
    Ok(messages)
}

/// 只读统计指定阶段中 `*.json` 文件的个数。
///
/// 不解析、不 reject、不创建目录（目录不存在返回 0），供 `--status` 等只读路径使用。
pub fn count_inbox(session: &Session, phase: InboxPhase) -> Result<usize> {
    let phase_dir = session.inbox_dir().join(phase.dir_name());
    let entries = match fs::read_dir(&phase_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    let count = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false)
                && entry.path().extension().and_then(|ext| ext.to_str()) == Some(EXTENSION)
        })
        .count();
    Ok(count)
}

/// 删除一条已消费的消息文件。
///
/// 文件已不存在时视为成功（幂等）：崩溃可能发生在 `append` 之后、删除之前，重新扫描
/// 时由调用方按 id 去重，故重复删除不报错。
pub fn remove_inbox(message: &InboxMessage) -> Result<()> {
    match fs::remove_file(&message.path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// 尽力清理 `tmp/` 中修改时间早于 `max_age` 的文件。
///
/// 只处理 `tmp/`，不触碰 `pending/`、`init/`、`rejected/`；任何错误静默忽略。
pub fn cleanup_inbox_tmp(session: &Session, max_age: Duration) {
    let tmp_dir = session.inbox_dir().join(TMP_DIR);
    let Ok(entries) = fs::read_dir(&tmp_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
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

/// 解析文件为 `Record`；要求 `.json` 扩展名且内容合法。
fn parse_candidate(path: &Path) -> Option<Record> {
    if path.extension().and_then(|ext| ext.to_str()) != Some(EXTENSION) {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 文件名去掉 `.json` 扩展名。
fn file_stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
}

/// 载荷是否允许注入。
fn is_acceptable_payload(record: &Record) -> bool {
    match record {
        Record::User(_) => true,
        Record::System(system) => system.source == "notify",
        _ => false,
    }
}

/// 消息 id 是否满足 `[A-Za-z0-9_-]{1,128}`。
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// 尽力把非法文件移入 `rejected/`；失败静默忽略。
fn reject(inbox: &Path, path: &Path, ready: &mut bool) {
    let rejected = inbox.join(REJECTED_DIR);
    if !*ready {
        if fs::create_dir_all(&rejected).is_err() {
            return;
        }
        *ready = true;
    }
    if let Some(name) = path.file_name() {
        let _ = fs::rename(path, rejected.join(name));
    }
}

/// 临时文件名：`<id>.<pid>.<counter>.json`，保证并发发布不撞名。
fn tmp_file_name(id: &str) -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{id}.{}.{counter}.{EXTENSION}", std::process::id())
}

/// 落盘并 `sync_all`。
fn write_synced(path: &Path, payload: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(payload)?;
    file.sync_all()?;
    Ok(())
}

/// fsync 目录项，保证 rename 后的目录元数据落盘。
fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use yaa_msg::User;

    use super::*;

    fn session_in(dir: &Path) -> Session {
        Session::new(dir.join("session.db"))
    }

    fn user_with_id(id: &str, content: &str) -> Record {
        Record::User(User {
            id: id.to_owned(),
            ts: 1,
            content: content.to_owned(),
            client_id: None,
            turn_id: None,
        })
    }

    fn write_raw(dir: &Path, name: &str, bytes: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), bytes).unwrap();
    }

    fn count_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .map(|entries| entries.flatten().filter(|e| e.path().is_file()).count())
            .unwrap_or(0)
    }

    #[test]
    fn concurrent_publish_has_no_loss_and_no_tmp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let threads = 64;
        let per_thread = 16;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let session = session.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let records: Vec<_> = (0..per_thread)
                        .map(|index| Record::user(format!("{thread}-{index}")))
                        .collect();
                    barrier.wait();
                    for record in &records {
                        publish_inbox(&session, InboxPhase::Pending, record).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let messages = scan_inbox(&session, InboxPhase::Pending).unwrap();
        assert_eq!(messages.len(), threads * per_thread);

        let ids: std::collections::HashSet<_> = messages
            .iter()
            .map(|message| message.id().to_owned())
            .collect();
        assert_eq!(ids.len(), messages.len(), "文件名不得重复");

        for message in &messages {
            let stem = message.path.file_stem().unwrap().to_str().unwrap();
            assert_eq!(stem, message.record.id(), "文件名必须等于 id");
        }

        assert_eq!(count_files(&session.inbox_dir().join(TMP_DIR)), 0);
    }

    #[test]
    fn publish_round_trips_user_and_notify() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        let user = Record::user("你好");
        let notify = Record::system("notify", "通知", Some("git status".into()), None);
        publish_inbox(&session, InboxPhase::Pending, &user).unwrap();
        publish_inbox(&session, InboxPhase::Pending, &notify).unwrap();

        let messages = scan_inbox(&session, InboxPhase::Pending).unwrap();
        assert_eq!(messages.len(), 2);
        let records: Vec<_> = messages.into_iter().map(|message| message.record).collect();
        assert!(records.contains(&user));
        assert!(records.contains(&notify));
    }

    #[test]
    fn scan_is_phase_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        publish_inbox(&session, InboxPhase::Pending, &Record::user("待处理")).unwrap();
        publish_inbox(
            &session,
            InboxPhase::Init,
            &Record::system("notify", "初始化", None, None),
        )
        .unwrap();

        assert_eq!(scan_inbox(&session, InboxPhase::Pending).unwrap().len(), 1);
        assert_eq!(scan_inbox(&session, InboxPhase::Init).unwrap().len(), 1);
    }

    #[test]
    fn illegal_payloads_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());

        let ai = Record::ai("m", "h", "回复", None);
        write_raw(
            &pending,
            &format!("{}.json", ai.id()),
            serde_json::to_vec(&ai).unwrap().as_slice(),
        );

        let user = Record::user("内容");
        write_raw(
            &pending,
            "mismatch.json",
            serde_json::to_vec(&user).unwrap().as_slice(),
        );

        let non_notify = Record::system("init", "簿记", None, None);
        write_raw(
            &pending,
            &format!("{}.json", non_notify.id()),
            serde_json::to_vec(&non_notify).unwrap().as_slice(),
        );

        let messages = scan_inbox(&session, InboxPhase::Pending).unwrap();
        assert!(messages.is_empty(), "非法载荷不得返回");

        let rejected = session.inbox_dir().join(REJECTED_DIR);
        assert_eq!(count_files(&rejected), 3, "非法文件应移入 rejected/");
        assert_eq!(count_files(&pending), 0);
    }

    #[test]
    fn unparsable_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());
        write_raw(&pending, "broken.json", b"{ not json");

        assert!(
            scan_inbox(&session, InboxPhase::Pending)
                .unwrap()
                .is_empty()
        );
        assert_eq!(count_files(&session.inbox_dir().join(REJECTED_DIR)), 1);
    }

    #[test]
    fn remove_inbox_deletes_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        publish_inbox(&session, InboxPhase::Pending, &Record::user("一")).unwrap();

        let messages = scan_inbox(&session, InboxPhase::Pending).unwrap();
        let message = &messages[0];
        assert!(message.path.exists());

        remove_inbox(message).unwrap();
        assert!(!message.path.exists());
        // 重复删除不 panic：文件已不存在时视为成功。
        remove_inbox(message).unwrap();
    }

    #[test]
    fn cleanup_only_clears_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let inbox = session.inbox_dir();

        publish_inbox(&session, InboxPhase::Pending, &Record::user("待处理")).unwrap();
        publish_inbox(
            &session,
            InboxPhase::Init,
            &Record::system("notify", "初始化", None, None),
        )
        .unwrap();
        write_raw(&inbox.join(REJECTED_DIR), "bad.json", b"bad");
        write_raw(&inbox.join(TMP_DIR), "leftover.json", b"tmp");

        cleanup_inbox_tmp(&session, Duration::ZERO);

        assert_eq!(count_files(&inbox.join(TMP_DIR)), 0, "tmp 应被清空");
        assert_eq!(count_files(&inbox.join("pending")), 1, "pending 不得被清理");
        assert_eq!(count_files(&inbox.join("init")), 1, "init 不得被清理");
        assert_eq!(
            count_files(&inbox.join(REJECTED_DIR)),
            1,
            "rejected 不得被清理"
        );
    }

    #[test]
    fn cleanup_keeps_fresh_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let inbox = session.inbox_dir();
        write_raw(&inbox.join(TMP_DIR), "fresh.json", b"tmp");

        cleanup_inbox_tmp(&session, Duration::from_secs(3600));

        assert_eq!(count_files(&inbox.join(TMP_DIR)), 1, "新文件不应被清理");
    }

    #[test]
    fn scan_returns_messages_sorted_by_filename() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());

        for id in [
            "01H0000000000000000000000C",
            "01H0000000000000000000000A",
            "01H0000000000000000000000B",
        ] {
            let record = user_with_id(id, id);
            write_raw(
                &pending,
                &format!("{id}.json"),
                serde_json::to_vec(&record).unwrap().as_slice(),
            );
        }

        let ids: Vec<_> = scan_inbox(&session, InboxPhase::Pending)
            .unwrap()
            .into_iter()
            .map(|message| message.id().to_owned())
            .collect();
        assert_eq!(
            ids,
            vec![
                "01H0000000000000000000000A",
                "01H0000000000000000000000B",
                "01H0000000000000000000000C",
            ]
        );
    }

    #[test]
    fn publish_rejects_illegal_ids_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let illegal = vec![
            "a/b".to_owned(),
            "..".to_owned(),
            String::new(),
            "x".repeat(MAX_ID_BYTES + 1),
            "a b".to_owned(),
            "a\\b".to_owned(),
        ];

        for id in illegal {
            let record = user_with_id(&id, "内容");
            let err = publish_inbox(&session, InboxPhase::Pending, &record).unwrap_err();
            assert!(matches!(err, StoreError::Inbox(_)), "id={id:?} 应被拒");
            assert!(!session.inbox_dir().exists(), "非法 id 不得创建 inbox 目录");
        }

        let boundary = user_with_id(&"x".repeat(MAX_ID_BYTES), "边界");
        publish_inbox(&session, InboxPhase::Pending, &boundary).unwrap();
        assert_eq!(count_files(&session.inbox_dir().join("pending")), 1);
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());
        write_raw(
            &pending,
            "oversized.json",
            &vec![b' '; MAX_INBOX_BYTES as usize + 1],
        );

        assert!(
            scan_inbox(&session, InboxPhase::Pending)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            count_files(&session.inbox_dir().join(REJECTED_DIR)),
            1,
            "超限文件应移入 rejected/"
        );
        assert_eq!(count_files(&pending), 0);
    }

    #[test]
    fn scan_rejects_payload_with_illegal_id() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());
        let record = user_with_id("a b", "内容");
        write_raw(
            &pending,
            "a b.json",
            serde_json::to_vec(&record).unwrap().as_slice(),
        );

        assert!(
            scan_inbox(&session, InboxPhase::Pending)
                .unwrap()
                .is_empty()
        );
        assert_eq!(count_files(&session.inbox_dir().join(REJECTED_DIR)), 1);
    }

    #[test]
    fn count_inbox_is_read_only_and_does_not_reject() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        assert_eq!(count_inbox(&session, InboxPhase::Pending).unwrap(), 0);
        assert!(!session.inbox_dir().exists(), "计数不得创建目录");

        let pending = session.inbox_dir().join(InboxPhase::Pending.dir_name());
        write_raw(&pending, "broken.json", b"{ not json");
        write_raw(&pending, "notes.txt", b"not inbox");
        assert_eq!(count_inbox(&session, InboxPhase::Pending).unwrap(), 1);
        assert!(
            !session.inbox_dir().join(REJECTED_DIR).exists(),
            "计数不得 reject 文件"
        );
        assert_eq!(count_files(&pending), 2, "计数不得移动文件");

        assert_eq!(count_inbox(&session, InboxPhase::Init).unwrap(), 0);
    }

    #[test]
    fn notify_payload_round_trips_with_command() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let notify = Record::system("notify", "输出", Some("seq 1 3".into()), None);
        publish_inbox(&session, InboxPhase::Init, &notify).unwrap();

        let messages = scan_inbox(&session, InboxPhase::Init).unwrap();
        assert_eq!(messages[0].record, notify);
    }
}
