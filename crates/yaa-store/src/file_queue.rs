//! inbox 与 spool 共用的原子文件发布原语。
//!
//! 两者都是「一个目录里的 `<记录 id>.json` 文件」：发布 = 写 `tmp/` → `sync_all` →
//! `rename` 到目标目录 → fsync 目录；消费后删除。本模块只放与具体语义无关的共享
//! 原语，避免 inbox 与 spool 的命名、id 校验与大小上限漂移。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use yaa_msg::Record;

use crate::{Result, StoreError};

/// 发布中间文件目录名。
pub(crate) const TMP_DIR: &str = "tmp";
/// 非法载荷目录名。
pub(crate) const REJECTED_DIR: &str = "rejected";
/// 消息文件扩展名。
pub(crate) const EXTENSION: &str = "json";
/// 单个消息文件的大小上限：超限视为非法载荷。
pub(crate) const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;
/// 消息 id 允许的最大字节数。
pub(crate) const MAX_ID_BYTES: usize = 128;

/// 进程内临时文件序号：即便并发发布同一条记录也不撞名。
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 发布一条记录到 `target_dir`，返回目标文件路径。
///
/// 先把序列化载荷原子地落到 `tmp_dir`，再 `rename` 到 `<target_dir>/<id>.json`；任何
/// 一步失败都会清理临时文件后返回错误。多进程无锁并发安全。
///
/// `tmp_dir` 由调用方指定：inbox 用共享的 `inbox/tmp/`（不污染各 phase 目录），spool
/// 用 `spool/tmp/`。
///
/// `record.id()` 必须匹配 `[A-Za-z0-9_-]{1,128}`，否则直接报 [`StoreError::Inbox`]，
/// 不拼接任何路径，避免 id 逃逸出目标目录。
pub(crate) fn publish_atomic(target_dir: &Path, tmp_dir: &Path, record: &Record) -> Result<()> {
    validate_id(record.id())?;

    fs::create_dir_all(tmp_dir)?;
    fs::create_dir_all(target_dir)?;

    let payload = serialize_record(record)?;

    let tmp_path = tmp_dir.join(tmp_file_name(record.id()));
    write_synced(&tmp_path, &payload)?;
    let target = target_dir.join(format!("{}.{EXTENSION}", record.id()));
    if let Err(err) = fs::rename(&tmp_path, &target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    fsync_dir(target_dir)?;
    Ok(())
}

/// 扫描 `dir` 中的合法消息文件，按文件名（ULID）升序返回。
///
/// 载荷必须是 `Record::User` 或可注入的 `System`，且文件名（去 `.json` 扩展名）必须
/// 等于 `record.id()`。解析失败、超限或任一校验不符的文件会被尽力移动到
/// `reject_dir` 并跳过——不返回、不报错。`tmp/` 不参与扫描。目录不存在时返回空。
pub(crate) fn scan_payloads(dir: &Path, reject_dir: &Path) -> Result<Vec<(PathBuf, Record)>> {
    let entries = match fs::read_dir(dir) {
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
            .map(|metadata| metadata.len() > MAX_PAYLOAD_BYTES)
            .unwrap_or(false);
        if oversized {
            reject(reject_dir, &path, &mut rejected_ready);
            continue;
        }
        let Some(record) = parse_candidate(&path) else {
            reject(reject_dir, &path, &mut rejected_ready);
            continue;
        };
        if !is_acceptable_payload(&record)
            || !is_valid_id(record.id())
            || record.id() != file_stem(&path)
        {
            reject(reject_dir, &path, &mut rejected_ready);
            continue;
        }
        messages.push((path, record));
    }
    messages.sort_by(|left, right| left.0.file_name().cmp(&right.0.file_name()));
    Ok(messages)
}

/// 只读统计 `dir` 中 `*.json` 文件的个数。
///
/// 不解析、不 reject、不创建目录（目录不存在返回 0），供 `--status` 等只读路径使用。
pub(crate) fn count_payloads(dir: &Path) -> Result<usize> {
    let entries = match fs::read_dir(dir) {
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

/// 删除一条已消费的消息文件；文件已不存在时视为成功（幂等）。
pub(crate) fn remove_payload(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// 尽力清理 `tmp/` 中修改时间早于 `max_age` 的文件。
///
/// 只处理 `tmp/`；任何错误静默忽略。
pub(crate) fn cleanup_tmp(tmp_dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(tmp_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
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

/// 校验并序列化记录；超过单条上限时拒绝，避免写出必然被扫描拒绝的文件。
pub(crate) fn serialize_record(record: &Record) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(record)
        .map_err(|err| StoreError::Inbox(format!("记录序列化失败：{err}")))?;
    if payload.len() as u64 > MAX_PAYLOAD_BYTES {
        return Err(StoreError::Inbox(format!(
            "记录 {} 超过单条消息上限 {} 字节",
            record.id(),
            MAX_PAYLOAD_BYTES
        )));
    }
    Ok(payload)
}

/// 消息 id 是否满足 `[A-Za-z0-9_-]{1,128}`。
pub(crate) fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// 校验消息 id；非法时返回 [`StoreError::Inbox`]。
fn validate_id(id: &str) -> Result<()> {
    if is_valid_id(id) {
        return Ok(());
    }
    Err(StoreError::Inbox(format!("非法消息 id：{id:?}")))
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

/// 载荷是否允许注入：普通 user 或可注入的系统通知。
pub(crate) fn is_acceptable_payload(record: &Record) -> bool {
    match record {
        Record::User(_) => true,
        Record::System(system) => yaa_msg::is_injectable_system(system),
        _ => false,
    }
}

/// 尽力把非法文件移入 `reject_dir`；失败静默忽略。
fn reject(reject_dir: &Path, path: &Path, ready: &mut bool) {
    if !*ready {
        if fs::create_dir_all(reject_dir).is_err() {
            return;
        }
        *ready = true;
    }
    if let Some(name) = path.file_name() {
        let _ = fs::rename(path, reject_dir.join(name));
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
