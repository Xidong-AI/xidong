//! 会话解析：由工作目录或显式路径得到 `yaa_store::Session`。
//!
//! 默认会话根为 `<数据根>/sessions/<sid>/`，数据库为其中的 `session.db`；`sid` 取
//! 规范化工作目录字节的 sha256 前 32 位十六进制（128 位）。`canonicalize` 失败
//! （目录已删、符号链接断裂）时回退到词法绝对路径并在 [`Resolved::warned`] 中标记，
//! 绝不静默。
//!
//! 显式路径统一绝对化后按以下规则解释：以 `.jsonl` 结尾视为旧版会话并报错；是目录
//! 时使用 `<目录>/session.db`；其余按数据库路径。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 一次会话解析结果。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 存储层会话句柄。
    pub session: yaa_store::Session,
    /// 会话 id；显式路径取数据库父目录名（无目录名时取路径哈希）。
    pub sid: String,
    /// 用于 sid 计算的绝对工作目录。
    pub cwd_absolute: PathBuf,
    /// 是否因 `canonicalize` 失败而回退到词法绝对路径。
    pub warned: bool,
}

/// 解析会话。
///
/// `session_arg` 给出时按 `session_from_path` 的规则构造 [`yaa_store::Session`]；否则
/// 按规范化工作目录计算 sid。本函数是只读的：不创建任何目录，目录创建交给真正写入的
/// [`yaa_store::append`]，使 `--status`/`--clear` 不会物化空的会话目录。
pub fn resolve(
    cwd: &Path,
    session_arg: Option<&Path>,
    data_root: &Path,
) -> anyhow::Result<Resolved> {
    let (cwd_absolute, warned) = absolute_cwd(cwd);

    if let Some(path) = session_arg {
        let session = session_from_path(path)?;
        let sid = sid_of(&session);
        return Ok(Resolved {
            session,
            sid,
            cwd_absolute,
            warned,
        });
    }

    let sid = sid_for(&cwd_absolute);
    let dir = data_root.join("sessions").join(&sid);
    let db = dir.join("session.db");
    reject_legacy_sibling(&db)?;
    let session = yaa_store::Session::new(db);
    Ok(Resolved {
        session,
        sid,
        cwd_absolute,
        warned,
    })
}

/// 把显式会话路径解释为数据库路径。
///
/// 相对路径先词法绝对化（与 `--notify` 的既有语义一致）；`.jsonl` 是旧版会话，明确
/// 报错并提示 `--clear`；目录则使用其中的 `session.db`。
fn session_from_path(path: &Path) -> anyhow::Result<yaa_store::Session> {
    let absolute = std::path::absolute(path)?;
    if is_legacy_jsonl(&absolute) {
        return Err(legacy_jsonl_error(&absolute));
    }
    let db = if absolute.is_dir() {
        let db = absolute.join("session.db");
        reject_legacy_sibling(&db)?;
        db
    } else {
        absolute
    };
    Ok(yaa_store::Session::new(db))
}

/// 目标数据库尚未创建、但同目录存在旧 `default.jsonl` 时，拒绝静默绕过旧历史。
///
/// 只在 `session.db` 不存在时检查：已迁移用户的旧文件残留无碍。
fn reject_legacy_sibling(db: &Path) -> anyhow::Result<()> {
    if db.exists() {
        return Ok(());
    }
    let Some(dir) = db.parent() else {
        return Ok(());
    };
    let legacy = dir.join("default.jsonl");
    if legacy.exists() {
        return Err(legacy_jsonl_error(&legacy));
    }
    Ok(())
}

/// 旧版 JSONL 会话的统一错误文案。
fn legacy_jsonl_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "检测到旧版 JSONL 会话：{}；请运行 `yaa --clear --session {}` 归档后使用 SQLite 会话",
        path.display(),
        path.display()
    )
}

/// 路径是否以 `.jsonl` 结尾（旧版会话格式）。
fn is_legacy_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

/// 解析会话目标，优先级：显式 `--session` → 环境变量 `YAA_SESSION`（非空才算）
/// → 当前 cwd 默认会话。
///
/// 显式路径相对当前 cwd 词法绝对化（与 `--notify` 的既有语义一致）；`YAA_SESSION`
/// 不属于「cwd 规范化回退」，故 `warned = false`、`cwd_absolute` 取当前工作目录。
/// 默认分支复用 [`resolve`]，不改变 `--clear`/`--status` 与普通 run 的既有行为。
pub fn resolve_target(
    cwd: &Path,
    session_arg: Option<&Path>,
    env_session: Option<&str>,
    data_root: &Path,
) -> anyhow::Result<Resolved> {
    if let Some(path) = session_arg {
        let session = session_from_path(path)?;
        let sid = sid_of(&session);
        let (cwd_absolute, warned) = absolute_cwd(cwd);
        return Ok(Resolved {
            session,
            sid,
            cwd_absolute,
            warned,
        });
    }
    if let Some(path) = env_session.filter(|value| !value.is_empty()) {
        let session = session_from_path(Path::new(path))?;
        let sid = sid_of(&session);
        return Ok(Resolved {
            session,
            sid,
            cwd_absolute: cwd.to_path_buf(),
            warned: false,
        });
    }
    resolve(cwd, None, data_root)
}

/// 会话 id；store 已按「父目录名 → 路径哈希」推导。
fn sid_of(session: &yaa_store::Session) -> String {
    session.sid.clone()
}

/// 会话输出目录：`<会话根>/outputs`。
pub fn outputs_dir(session: &yaa_store::Session) -> PathBuf {
    session.root.join("outputs")
}

/// 规范化工作目录；失败时回退到词法绝对路径并返回 `warned = true`。
fn absolute_cwd(cwd: &Path) -> (PathBuf, bool) {
    match cwd.canonicalize() {
        Ok(path) => (path, false),
        Err(_) => match std::path::absolute(cwd) {
            Ok(path) => (path, true),
            // 词法绝对化在极端情况下也可能失败（如 cwd 为空）；此时保留原路径。
            Err(_) => (cwd.to_path_buf(), true),
        },
    }
}

/// 计算工作目录对应的 32 位十六进制 sid。
pub fn sid_for(cwd_absolute: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd_absolute.as_os_str().as_encoded_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lower_hex(value: &str) -> bool {
        value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn sid_is_stable_and_32_hex() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let first = resolve(&cwd, None, &data).unwrap();
        let second = resolve(&cwd, None, &data).unwrap();

        assert!(is_lower_hex(&first.sid));
        assert_eq!(first.sid, second.sid);
        assert!(!first.warned);
        assert_eq!(first.session.root, data.join("sessions").join(&first.sid));
        assert_eq!(first.session.db, first.session.root.join("session.db"));
        // 解析是只读的：--status/--clear 不得物化空会话目录，写入时才创建。
        assert!(
            !first.session.root.exists(),
            "resolve 不得创建会话目录：{}",
            first.session.root.display()
        );
        assert_eq!(
            outputs_dir(&first.session),
            first.session.root.join("outputs")
        );
    }

    #[test]
    fn different_cwd_yields_different_sid() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let resolved_a = resolve(&a, None, &data).unwrap();
        let resolved_b = resolve(&b, None, &data).unwrap();
        assert_ne!(resolved_a.sid, resolved_b.sid);
    }

    #[test]
    fn explicit_session_arg_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let db = dir.path().join("custom").join("my.db");

        let resolved = resolve(&cwd, Some(&db), &dir.path().join("data")).unwrap();
        assert_eq!(resolved.session.db, db);
        assert_eq!(resolved.session.root, dir.path().join("custom"));
        assert_eq!(resolved.sid, "custom");
        assert!(!resolved.warned);
    }

    #[test]
    fn explicit_directory_uses_session_db_inside() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = dir.path().join("custom");
        std::fs::create_dir_all(&target).unwrap();

        let resolved = resolve(&cwd, Some(&target), &dir.path().join("data")).unwrap();
        assert_eq!(resolved.session.db, target.join("session.db"));
        assert_eq!(resolved.sid, "custom");
    }

    #[test]
    fn default_session_rejects_legacy_default_jsonl_when_db_missing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        // 先算出默认会话目录，放入旧 `default.jsonl`。
        let sid = sid_for(&cwd.canonicalize().unwrap());
        let session_dir = data.join("sessions").join(&sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        let legacy = session_dir.join("default.jsonl");
        std::fs::write(&legacy, b"{}\n").unwrap();

        let err = resolve(&cwd, None, &data).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("旧版 JSONL"), "{message}");
        assert!(message.contains("--clear"), "{message}");
        assert!(
            message.contains(&legacy.to_string_lossy().to_string()),
            "{message}"
        );
    }

    #[test]
    fn explicit_directory_rejects_legacy_default_jsonl_when_db_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = dir.path().join("custom");
        std::fs::create_dir_all(&target).unwrap();
        let legacy = target.join("default.jsonl");
        std::fs::write(&legacy, b"{}\n").unwrap();

        let err = resolve(&cwd, Some(&target), &dir.path().join("data")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("旧版 JSONL"), "{message}");
        assert!(message.contains("--clear"), "{message}");
        assert!(
            message.contains(&legacy.to_string_lossy().to_string()),
            "{message}"
        );
    }

    #[test]
    fn existing_session_db_allows_legacy_sibling_to_remain() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = dir.path().join("custom");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("default.jsonl"), b"{}\n").unwrap();
        std::fs::write(target.join("session.db"), b"x").unwrap();

        let resolved = resolve(&cwd, Some(&target), &dir.path().join("data")).unwrap();
        assert_eq!(resolved.session.db, target.join("session.db"));
    }

    #[test]
    fn explicit_jsonl_is_rejected_with_clear_hint() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let legacy = dir.path().join("old.jsonl");

        let err = resolve(&cwd, Some(&legacy), &dir.path().join("data")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("旧版 JSONL"), "{message}");
        assert!(message.contains("--clear"), "{message}");
    }

    #[test]
    fn resolve_target_prefers_explicit_then_env_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let explicit = dir.path().join("explicit.db");
        let from_env = dir.path().join("env").join("from-env.db");

        // 显式参数优先于环境变量。
        let resolved = resolve_target(
            &cwd,
            Some(&explicit),
            Some(from_env.to_str().unwrap()),
            &data,
        )
        .unwrap();
        assert_eq!(resolved.session.db, explicit);

        // 无显式参数时用 YAA_SESSION；显式路径不算 cwd 规范化回退，故不告警。
        let resolved = resolve_target(&cwd, None, Some(from_env.to_str().unwrap()), &data).unwrap();
        assert_eq!(resolved.session.db, from_env);
        assert!(!resolved.warned);
        assert_eq!(resolved.cwd_absolute, cwd);

        // 空串环境变量视为未设置，回退 cwd 默认会话。
        let resolved = resolve_target(&cwd, None, Some(""), &data).unwrap();
        assert_eq!(
            resolved.session.root,
            data.join("sessions").join(&resolved.sid)
        );
        assert_eq!(
            resolved.session.db,
            resolved.session.root.join("session.db")
        );
        assert!(is_lower_hex(&resolved.sid));
    }

    #[test]
    fn missing_cwd_falls_back_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let missing = dir.path().join("does-not-exist");

        let resolved = resolve(&missing, None, &data).unwrap();
        assert!(resolved.warned);
        assert!(is_lower_hex(&resolved.sid));
        // 回退路径是词法绝对路径，仍然稳定。
        assert_eq!(resolved.sid, sid_for(&resolved.cwd_absolute));
    }
}
