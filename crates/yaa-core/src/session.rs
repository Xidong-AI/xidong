//! 会话解析：由工作目录或显式路径得到 `yaa_store::Session`。
//!
//! 默认会话根为 `<数据根>/sessions/<sid>/`，`sid` 取规范化工作目录字节的
//! sha256 前 32 位十六进制（128 位）。`canonicalize` 失败（目录已删、符号链接断裂）
//! 时回退到词法绝对路径并在 [`Resolved::warned`] 中标记，绝不静默。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 一次会话解析结果。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 存储层会话句柄。
    pub session: yaa_store::Session,
    /// 会话 id；显式 `--session` 时取会话根目录名。
    pub sid: String,
    /// 用于 sid 计算的绝对工作目录。
    pub cwd_absolute: PathBuf,
    /// 是否因 `canonicalize` 失败而回退到词法绝对路径。
    pub warned: bool,
}

/// 解析会话。
///
/// `session_arg` 给出时直接以其构造 [`yaa_store::Session`]；否则按规范化工作目录计算
/// sid。本函数是只读的：不创建任何目录，目录创建交给真正写入的
/// [`yaa_store::append`]，使 `--status`/`--clear` 不会物化空的会话目录。
pub fn resolve(
    cwd: &Path,
    session_arg: Option<&Path>,
    data_root: &Path,
) -> anyhow::Result<Resolved> {
    let (cwd_absolute, warned) = absolute_cwd(cwd);

    if let Some(path) = session_arg {
        let session = yaa_store::Session::new(path);
        let sid = session
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok(Resolved {
            session,
            sid,
            cwd_absolute,
            warned,
        });
    }

    let sid = sid_for(&cwd_absolute);
    let dir = data_root.join("sessions").join(&sid);
    let session = yaa_store::Session::new(dir.join("default.jsonl"));
    Ok(Resolved {
        session,
        sid,
        cwd_absolute,
        warned,
    })
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
        assert_eq!(first.session.file, first.session.root.join("default.jsonl"));
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
        let file = dir.path().join("custom").join("my.jsonl");

        let resolved = resolve(&cwd, Some(&file), &dir.path().join("data")).unwrap();
        assert_eq!(resolved.session.file, file);
        assert_eq!(resolved.session.root, dir.path().join("custom"));
        assert_eq!(resolved.sid, "custom");
        assert!(!resolved.warned);
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
