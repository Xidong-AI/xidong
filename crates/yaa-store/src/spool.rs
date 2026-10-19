//! spool：通知发布失败时的保底暂存区。
//!
//! 位置与 inbox 同级：`<会话根>/<artifact 前缀>spool/<记录 id>.json`。载荷、命名、
//! id 校验与 1 MiB 上限完全复用 [`crate::file_queue`] 的原子发布原语；区别只在于
//! spool 没有 phase 子目录，且由 runner 在 init 之前补投。
//!
//! 补投成功后删除文件；崩溃在「已 append、未删除」之间时文件会重现，由
//! `append_idempotent` 按 id 去重。

use std::path::PathBuf;
use std::time::Duration;

use yaa_msg::Record;

use crate::file_queue::{self, EXTENSION, REJECTED_DIR, TMP_DIR};
use crate::{Result, Session};

/// 扫描出的一条 spool 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolMessage {
    /// 消息文件路径。
    pub path: PathBuf,
    /// 解析出的记录。
    pub record: Record,
}

impl SpoolMessage {
    /// 消息 id（等于文件名去扩展名）。
    pub fn id(&self) -> &str {
        self.record.id()
    }
}

/// 把一条记录原子写入 spool，返回目标文件路径。
///
/// 与 inbox 共用命名、id 与大小校验；同一 id 重复发布时覆盖同名文件（幂等）。
/// 本函数不校验 generation/archived；需要与 `--clear` 协调时用
/// [`publish_spool_with_generation`]。
pub fn publish_spool(session: &Session, record: &Record) -> Result<PathBuf> {
    publish_spool_with_generation(session, record, None)
}

/// 带 generation/archived 守卫地把通知写入 spool。
///
/// 与 [`crate::publish_with_generation`] 同锁序：先持 `clear.lock` SH（与 `--clear`
/// 的 EX 互斥），再读取 generation/archived。`expected` 语义一致（`0 -> 1` 允许首次
/// 建库）；已归档或 generation 变化时拒绝写入，避免 spool 落在已归档/新 generation
/// 下绕过 `--clear` 的 spool 拒绝检查（F10）。
///
/// 读取 generation/archived 若遇 BUSY/IO，视为「暂时不可判定」：`clear.lock` SH 已
/// 排除并发 `--clear`，仍写 spool 以免通知丢失（F9 的保底语义）。
pub fn publish_spool_with_generation(
    session: &Session,
    record: &Record,
    expected: Option<i64>,
) -> Result<PathBuf> {
    crate::ensure_root(session)?;
    let _clear = crate::FileLock::shared(&session.clear_lock_path())?;
    let current = match crate::generation(session) {
        Ok(current) => Some(current),
        Err(err) if err.is_retryable() => None,
        Err(err) => return Err(err),
    };
    if let (Some(expected), Some(current)) = (expected, current) {
        let first_init = expected == 0 && current == 1;
        if current != expected && !first_init {
            return Err(crate::StoreError::GenerationChanged { expected, current });
        }
    }
    match crate::is_archived(session) {
        Ok(true) => {
            return Err(crate::StoreError::Archived {
                path: session.db.clone(),
            });
        }
        Ok(false) => {}
        Err(err) if err.is_retryable() => {}
        Err(err) => return Err(err),
    }

    let dir = session.spool_dir();
    file_queue::publish_atomic(&dir, &dir.join(TMP_DIR), record)?;
    Ok(dir.join(format!("{}.{EXTENSION}", record.id())))
}

/// 扫描 spool 中的合法消息，按文件名（ULID）升序返回。
///
/// 非法载荷与 inbox 一致：尽力移入 `spool/rejected/` 并跳过，不返回、不报错。
/// 目录不存在时返回空。
pub fn scan_spool(session: &Session) -> Result<Vec<SpoolMessage>> {
    let dir = session.spool_dir();
    Ok(file_queue::scan_payloads(&dir, &dir.join(REJECTED_DIR))?
        .into_iter()
        .map(|(path, record)| SpoolMessage { path, record })
        .collect())
}

/// 只读统计 spool 中 `*.json` 文件的个数；不解析、不 reject、不创建目录。
pub fn count_spool(session: &Session) -> Result<usize> {
    file_queue::count_payloads(&session.spool_dir())
}

/// 删除一条已补投的消息文件；文件已不存在时视为成功（幂等）。
pub fn remove_spool(message: &SpoolMessage) -> Result<()> {
    file_queue::remove_payload(&message.path)
}

/// 尽力清理 `spool/tmp/` 中修改时间早于 `max_age` 的文件；错误静默忽略。
pub fn cleanup_spool_tmp(session: &Session, max_age: Duration) {
    file_queue::cleanup_tmp(&session.spool_dir().join(TMP_DIR), max_age);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use yaa_msg::User;

    use super::*;

    fn session_in(dir: &Path) -> Session {
        Session::new(dir.join("session.db"))
    }

    fn user_with_id(id: &str) -> Record {
        Record::User(User {
            id: id.to_owned(),
            ts: 1,
            content: "内容".to_owned(),
            client_id: None,
            turn_id: None,
        })
    }

    fn count_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_file())
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn publish_and_scan_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let notify = Record::system_with_attribution(
            "notify",
            "<注释>\n正文\n</注释>",
            Some("echo hi".into()),
            None,
            Some("/data/self/session.db".into()),
            Some("call_1".into()),
        );

        let path = publish_spool(&session, &notify).unwrap();
        assert_eq!(
            path,
            session.spool_dir().join(format!("{}.json", notify.id()))
        );
        assert_eq!(count_spool(&session).unwrap(), 1);

        let messages = scan_spool(&session).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].record, notify);
        assert_eq!(messages[0].id(), notify.id());
    }

    #[test]
    fn scan_is_sorted_by_id_and_rejects_illegal_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let spool = session.spool_dir();
        fs::create_dir_all(&spool).unwrap();

        for id in [
            "01H0000000000000000000000C",
            "01H0000000000000000000000A",
            "01H0000000000000000000000B",
        ] {
            let record = user_with_id(id);
            fs::write(
                spool.join(format!("{id}.json")),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        }
        // 载荷类型不允许、文件名与 id 不一致、无法解析。
        let ai = Record::ai("m", "h", "回复", None);
        fs::write(
            spool.join(format!("{}.json", ai.id())),
            serde_json::to_vec(&ai).unwrap(),
        )
        .unwrap();
        let mismatch = user_with_id("01H0000000000000000000000D");
        fs::write(
            spool.join("mismatch.json"),
            serde_json::to_vec(&mismatch).unwrap(),
        )
        .unwrap();
        fs::write(spool.join("broken.json"), b"{ not json").unwrap();

        let ids: Vec<_> = scan_spool(&session)
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
        assert_eq!(
            count_files(&spool.join(REJECTED_DIR)),
            3,
            "非法文件应移入 spool/rejected/"
        );
    }

    #[test]
    fn publish_rejects_illegal_ids_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        let record = user_with_id("a/b");
        let err = publish_spool(&session, &record).unwrap_err();
        assert!(matches!(err, crate::StoreError::Inbox(_)), "{err}");
        assert!(!session.spool_dir().exists(), "非法 id 不得创建 spool 目录");
    }

    #[test]
    fn oversized_payload_is_rejected_by_scan() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let spool = session.spool_dir();
        fs::create_dir_all(&spool).unwrap();
        fs::write(
            spool.join("oversized.json"),
            vec![b' '; file_queue::MAX_PAYLOAD_BYTES as usize + 1],
        )
        .unwrap();

        assert!(scan_spool(&session).unwrap().is_empty());
        assert_eq!(count_files(&spool.join(REJECTED_DIR)), 1);
    }

    #[test]
    fn count_is_read_only_and_does_not_reject() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());

        assert_eq!(count_spool(&session).unwrap(), 0);
        assert!(!session.spool_dir().exists(), "计数不得创建目录");

        let spool = session.spool_dir();
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("broken.json"), b"{ not json").unwrap();
        fs::write(spool.join("notes.txt"), b"not spool").unwrap();
        assert_eq!(count_spool(&session).unwrap(), 1);
        assert!(!spool.join(REJECTED_DIR).exists(), "计数不得 reject 文件");
        assert_eq!(count_files(&spool), 2, "计数不得移动文件");
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let notify = Record::system("notify", "通知", None, None);
        publish_spool(&session, &notify).unwrap();

        let message = &scan_spool(&session).unwrap()[0];
        assert!(message.path.exists());
        remove_spool(message).unwrap();
        assert!(!message.path.exists());
        remove_spool(message).unwrap();
    }

    #[test]
    fn cleanup_only_clears_spool_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let spool = session.spool_dir();
        publish_spool(&session, &Record::system("notify", "通知", None, None)).unwrap();
        fs::create_dir_all(spool.join(TMP_DIR)).unwrap();
        fs::create_dir_all(spool.join(REJECTED_DIR)).unwrap();
        fs::write(spool.join(TMP_DIR).join("leftover.json"), b"tmp").unwrap();
        fs::write(spool.join(REJECTED_DIR).join("bad.json"), b"bad").unwrap();

        cleanup_spool_tmp(&session, Duration::ZERO);

        assert_eq!(count_files(&spool.join(TMP_DIR)), 0, "tmp 应被清空");
        assert_eq!(count_files(&spool), 1, "spool 根文件不得被清理");
        assert_eq!(
            count_files(&spool.join(REJECTED_DIR)),
            1,
            "rejected 不得被清理"
        );
    }
}

#[cfg(test)]
mod lock_tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn session_in(dir: &Path) -> Session {
        Session::new(dir.join("session.db"))
    }

    /// F10：`--clear` 持 `clear.lock` EX 时，spool 写入必须等待 SH，不能抢跑。
    #[test]
    fn publish_spool_waits_for_clear_exclusive_lock() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let clear = crate::FileLock::exclusive(&session.clear_lock_path()).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let worker_session = session.clone();
        let worker = std::thread::spawn(move || {
            let record = Record::system("notify", "锁序", None, None);
            tx.send(publish_spool(&worker_session, &record)).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "clear EX 持有时 spool 写入应等待"
        );
        drop(clear);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("释放 clear 后 spool 写入应完成")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(count_spool(&session).unwrap(), 1);
    }

    /// F10：generation 变化（模拟 `--clear`）时拒绝写 spool，避免绕过 clear 检查。
    #[test]
    fn publish_spool_with_generation_rejects_changed_generation() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        crate::append(&session, &[Record::user("既有")]).unwrap();
        let expected = crate::generation(&session).unwrap();

        let conn = rusqlite::Connection::open(&session.db).unwrap();
        conn.execute(
            "UPDATE session SET generation = generation + 1 WHERE id = 1",
            [],
        )
        .unwrap();
        drop(conn);

        let err = publish_spool_with_generation(
            &session,
            &Record::system("notify", "已换代", None, None),
            Some(expected),
        )
        .unwrap_err();
        assert!(
            matches!(err, crate::StoreError::GenerationChanged { .. }),
            "{err:?}"
        );
        assert_eq!(count_spool(&session).unwrap(), 0, "不得写入 spool");
    }

    /// F10：已归档会话拒绝写 spool。
    #[test]
    fn publish_spool_with_generation_rejects_archived() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        crate::append(&session, &[Record::user("既有")]).unwrap();
        let expected = crate::generation(&session).unwrap();
        crate::set_archived(&session).unwrap();

        let err = publish_spool_with_generation(
            &session,
            &Record::system("notify", "已归档", None, None),
            Some(expected),
        )
        .unwrap_err();
        assert!(matches!(err, crate::StoreError::Archived { .. }), "{err:?}");
        assert_eq!(count_spool(&session).unwrap(), 0, "不得写入 spool");
    }
}
