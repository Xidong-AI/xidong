//! 会话解析：由工作目录或显式路径得到 `crate::store::Session`。
//!
//! 默认会话根为 `<数据根>/sessions/<sid>/`，数据库为其中的 `session.db`；`sid` 取
//! 规范化工作目录字节的 sha256 前 32 位十六进制（128 位）。`canonicalize` 失败
//! （目录已删、符号链接断裂）时回退到词法绝对路径并在 [`Resolved::warned`] 中标记，
//! 绝不静默。
//!
//! 显式路径统一绝对化后按以下规则解释：以 `.jsonl` 结尾视为旧版会话并报错；是目录
//! 时使用 `<目录>/session.db`；其余按数据库路径。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 一次会话解析结果。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 存储层会话句柄。
    pub session: crate::store::Session,
    /// 会话 id；显式路径取数据库父目录名（无目录名时取路径哈希）。
    pub sid: String,
    /// 用于 sid 计算的绝对工作目录。
    pub cwd_absolute: PathBuf,
    /// 是否因 `canonicalize` 失败而回退到词法绝对路径。
    pub warned: bool,
}

/// 解析会话。
///
/// `session_arg` 给出时按 `session_from_path` 的规则构造 [`crate::store::Session`]；否则
/// 按规范化工作目录计算 sid。本函数是只读的：不创建任何目录，目录创建交给真正写入的
/// [`crate::store::append`]，使 `--status`/`--clear` 不会物化空的会话目录。
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
    let session = crate::store::Session::new(db);
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
fn session_from_path(path: &Path) -> anyhow::Result<crate::store::Session> {
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
    Ok(crate::store::Session::new(db))
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
        "检测到旧版 JSONL 会话：{}；请运行 `xidong --clear --session {}` 归档后使用 SQLite 会话",
        path.display(),
        path.display()
    )
}

/// 路径是否以 `.jsonl` 结尾（旧版会话格式）。
fn is_legacy_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

/// 解析会话目标，优先级：显式 `--session` → 环境变量 `XIDONG_SESSION`（非空才算）
/// → 当前 cwd 默认会话。
///
/// 显式路径相对当前 cwd 词法绝对化（与 `--notify` 的既有语义一致）；`XIDONG_SESSION`
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
        // 与 notify 路径统一：相对 `XIDONG_SESSION` 按当前 cwd 绝对化，避免后续
        // 子进程 cwd 变化时指向漂移。
        return resolved_from_env_path(path, cwd);
    }
    resolve(cwd, None, data_root)
}

/// 把显式路径（`--session`）解析为 [`Resolved`]；`cwd_absolute` 按规范化工作目录计算。
///
/// 相对路径按传入的 `cwd` 解析为绝对（而非进程当前目录），保证 `--cwd` 与测试注入的
/// 工作目录一致。
fn resolved_from_session_path(path: &Path, cwd: &Path) -> anyhow::Result<Resolved> {
    let session = session_from_path(&absolutize_from_cwd(path, cwd))?;
    let (cwd_absolute, warned) = absolute_cwd(cwd);
    Ok(Resolved {
        sid: sid_of(&session),
        session,
        cwd_absolute,
        warned,
    })
}

/// 把环境变量中的会话路径解析为 [`Resolved`]；相对值按 `cwd` 解析为绝对。
///
/// 环境变量不属于 cwd 规范化回退，故 `warned = false`、`cwd_absolute` 取当前工作目录。
fn resolved_from_env_path(path: &str, cwd: &Path) -> anyhow::Result<Resolved> {
    let session = session_from_path(&absolutize_from_cwd(Path::new(path), cwd))?;
    Ok(Resolved {
        sid: sid_of(&session),
        session,
        cwd_absolute: cwd.to_path_buf(),
        warned: false,
    })
}

/// 以 `cwd` 为基准把路径绝对化：绝对路径原样返回；相对路径拼到 `cwd` 后做词法归一化。
///
/// 入口绝对化（阶段 6 M5）：`XIDONG_NOTIFY`/`XIDONG_PARENT` 等环境变量或相对 `--to` 的
/// 非空值不再依赖进程当前目录，避免 `--cwd` 与测试注入的工作目录产生歧义。
fn absolutize_from_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf())
    };
    lexical_normalize(&base.join(path))
}

/// `--notify` 的目标解析结果。
///
/// [`Resolved`] 只描述「哪个会话」；本类型额外区分目标来源，供发布阶段决定是否
/// 严格创建（显式 `--to` 绝不创建）以及是否允许写入 `inbox/init/`（仅自发发布）。
#[derive(Debug, Clone)]
pub struct NotifyTarget {
    /// 解析后的会话。
    pub resolved: Resolved,
    /// 目标由显式 `--to` 指定：已通过严格校验，发布时不得创建任何目标文件。
    pub explicit: bool,
    /// 目标是否为本进程的自身会话（用于 `XIDONG_INBOX_PHASE=init` 判定）。
    ///
    /// 显式 `--to` 与 `XIDONG_NOTIFY` 选中的目标一律视为跨会话；`--session` 需与继承的
    /// `XIDONG_SESSION` 相同才算自身。
    pub is_self: bool,
}

/// [`resolve_notify_target`] 的错误：用法问题（退出码 2）与运行期问题（退出码 1）。
#[derive(Debug, thiserror::Error)]
pub enum NotifyTargetError {
    /// 参数组合或取值非法。
    #[error("{0}")]
    Usage(String),
    /// 目标不存在、旧格式、非 xidong 库、已归档或 IO 错误。
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// 解析 `--notify` 的目标会话（阶段 5，spec 第 2 节）。
///
/// 优先级从高到低：显式 `--to` → `--session` → `XIDONG_NOTIFY`（非空且不等于自身
/// `XIDONG_SESSION`）→ `XIDONG_SESSION`（非空）→ 当前 cwd 默认会话。
///
/// 只有显式 `--to` 走严格校验（存在、目录内的 `session.db`、旧 `.jsonl`、xidong 库、
/// 未归档），且**绝不创建**目标目录或文件；其余来源保持「不存在则警告并创建」的
/// 既有语义。
pub fn resolve_notify_target(
    cwd: &Path,
    to: Option<&Path>,
    session: Option<&Path>,
    data_root: &Path,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<NotifyTarget, NotifyTargetError> {
    if let Some(to) = to {
        if to.as_os_str().is_empty() {
            return Err(NotifyTargetError::Usage("`--to` 不能为空串。".into()));
        }
        // 相对 `--to`（常见于 `--to "$XIDONG_PARENT"` 展开失败或手输）按 cwd 解析为绝对，
        // 与 `XIDONG_NOTIFY`/`XIDONG_PARENT` 的入口绝对化一致，绝不静默拒绝。
        let to = absolutize_from_cwd(to, cwd);
        if let Some(session) = session {
            let to_db = lexical_db_path(&to)?;
            let session_db = lexical_db_path(session)?;
            if !same_session_path(&to_db, &session_db) {
                return Err(NotifyTargetError::Usage(format!(
                    "`--to` 与 `--session` 指向不同的会话：{} 与 {}。",
                    to_db.display(),
                    session_db.display()
                )));
            }
        }
        let session = explicit_target_session(&to)?;
        let (cwd_absolute, warned) = absolute_cwd(cwd);
        let resolved = Resolved {
            sid: sid_of(&session),
            session,
            cwd_absolute,
            warned,
        };
        return Ok(NotifyTarget {
            resolved,
            explicit: true,
            // 显式 `--to` 即使指向自身也按跨会话处理：一律写目标 `pending/`。
            is_self: false,
        });
    }

    let self_env = env("XIDONG_SESSION").filter(|value| !value.is_empty());
    let notify_env = env("XIDONG_NOTIFY").filter(|value| !value.is_empty());
    // 相对环境值按 cwd 解析后再比较，避免「相对/绝对指向同一会话」被误判为跨会话。
    let self_env_path = self_env
        .as_deref()
        .map(|path| absolutize_from_cwd(Path::new(path), cwd));
    let notify_is_self = match (notify_env.as_deref(), self_env_path.as_deref()) {
        (Some(notify), Some(self_path)) => {
            same_session_path(&absolutize_from_cwd(Path::new(notify), cwd), self_path)
        }
        _ => false,
    };

    let (resolved, source) = if let Some(path) = session {
        (
            resolved_from_session_path(path, cwd)?,
            TargetSource::Session,
        )
    } else if let Some(path) = notify_env.as_deref().filter(|_| !notify_is_self) {
        (resolved_from_env_path(path, cwd)?, TargetSource::NotifyEnv)
    } else if let Some(path) = self_env.as_deref() {
        (resolved_from_env_path(path, cwd)?, TargetSource::SessionEnv)
    } else {
        (resolve(cwd, None, data_root)?, TargetSource::Default)
    };

    let is_self = match source {
        // 显式 `--to` 已在上面返回；`XIDONG_NOTIFY` 只会在指向别的会话时胜出。
        TargetSource::NotifyEnv => false,
        TargetSource::Session | TargetSource::SessionEnv | TargetSource::Default => {
            match self_env_path.as_deref() {
                Some(self_path) => same_session_path(self_path, &resolved.session.db),
                // 没有继承 `XIDONG_SESSION` 时，本次解析出的目标就是自身会话。
                None => true,
            }
        }
    };

    Ok(NotifyTarget {
        resolved,
        explicit: false,
        is_self,
    })
}

/// 目标来源（仅用于 `is_self` 判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetSource {
    Session,
    NotifyEnv,
    SessionEnv,
    Default,
}

/// 显式 `--to` 的严格校验阶梯（顺序固定，只报第一个）。
///
/// 1. 绝对化后存在；
/// 2. 目录 → `<dir>/session.db` 存在；
/// 3. `.jsonl` 旧格式；
/// 4. 必须是普通文件（目录/FIFO/设备等按「不是 xidong 会话数据库」拒绝，绝不打开）；
/// 5. SQLite 文件头；
/// 6. 只读校验 `application_id`/`user_version` 后再查 archived（不经 init 窗口重试、
///    不创建任何锁文件）；
/// 7. canonicalize 目标库路径，使后续 inbox/spool 写到真实会话根（F8）。
///
/// 本函数只读：绝不创建目标目录、数据库或锁文件。
fn explicit_target_session(path: &Path) -> anyhow::Result<crate::store::Session> {
    let absolute = std::path::absolute(path)?;
    if !absolute.exists() {
        return Err(target_missing(&absolute));
    }

    let db = if absolute.is_dir() {
        let db = absolute.join("session.db");
        if !db.exists() {
            return Err(target_missing(&db));
        }
        db
    } else {
        if is_legacy_jsonl(&absolute) {
            return Err(legacy_jsonl_error(&absolute));
        }
        absolute
    };

    // F7：FIFO/设备等非普通文件绝不能进入 `File::open`（FIFO 会永久阻塞）。
    if !std::fs::metadata(&db)?.is_file() {
        return Err(crate::store::StoreError::NotXidongSession { path: db }.into());
    }

    let session = crate::store::Session::new(db);
    // 严格校验要求目标已是初始化过的 xidong 库：0 字节或非 SQLite 文件都不是。
    // 先看文件头，避免把「文件不是数据库」的底层 SQLite 错误暴露给用户。
    let mut header = [0u8; 16];
    let read = std::fs::File::open(&session.db)?.read(&mut header)?;
    if read < header.len() || header != *b"SQLite format 3\0" {
        return Err(crate::store::StoreError::NotXidongSession {
            path: session.db.clone(),
        }
        .into());
    }
    // F6：只读查询，不触发 `is_archived` 的 init 窗口重试（那会创建 `init.lock`）。
    if crate::store::archived_readonly(&session)? {
        anyhow::bail!("会话已归档，不可写入：{}", session.db.display());
    }
    // F5/F8：符号链接/`..` 一律归一到真实路径，保证与 `--session` 判等及 inbox 落点一致。
    let canonical = session.db.canonicalize()?;
    Ok(crate::store::Session::new(canonical))
}

/// 目标不存在时的统一错误文案。
fn target_missing(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "目标会话不存在：{}；显式 --to 不会创建新会话",
        path.display()
    )
}

/// 仅用于比较的数据库路径：绝对化，目录则取其中的 `session.db`；不检查存在性。
fn lexical_db_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    Ok(if absolute.is_dir() {
        absolute.join("session.db")
    } else {
        absolute
    })
}

/// 两个会话路径是否指向同一数据库。
///
/// 已存在的路径先 `canonicalize`（解析符号链接与 `..`）；不存在的路径退化为词法
/// 绝对化 + 去除 `.`/`..`。避免 `--to` 用符号链接或 `..` 表达时误报「指向不同的会话」。
fn same_session_path(left: &Path, right: &Path) -> bool {
    normalize_for_compare(left) == normalize_for_compare(right)
}

/// 用于比较的路径归一化：优先真实路径，失败时做词法归一化。
fn normalize_for_compare(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    lexical_normalize(&absolute)
}

/// 词法归一化：去掉 `.`，按 `..` 回退一级（不访问文件系统）。
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// 会话 id；store 已按「父目录名 → 路径哈希」推导。
fn sid_of(session: &crate::store::Session) -> String {
    session.sid.clone()
}

/// 会话输出目录：默认会话 `<会话根>/outputs`；显式会话再嵌一层 artifact 名
/// （`<会话根>/outputs/<artifact_name>`），使同一目录下多个显式会话的落盘输出互不混放。
pub fn outputs_dir(session: &crate::store::Session) -> PathBuf {
    let base = session.root.join("outputs");
    let name = session.artifact_name();
    if name.is_empty() {
        base
    } else {
        base.join(name)
    }
}

/// 构造本进程对子进程注入的 xidong 环境变量（阶段 5 环境链，见 spec 第 1 节）。
///
/// 单一来源：钩子（[`crate::core::agent::Runner`] 的 `hook_env`）、shell 工具 `extra_env`、
/// notify 子进程都调用本函数，消除「会话变量一处拼、限额变量另一处拼」的漂移。
///
/// - `self_db`：本会话库路径，输出前绝对化（失败时退回原路径）。
/// - `incoming`：当前进程继承的环境 getter（通常为 `std::env::var`）。
/// - `call_id`：当前 shell 工具调用的 id；仅 shell 工具路径传入 `Some`，钩子传 `None`。
///
/// 规则：
/// - `XIDONG_SESSION` 无条件重设为本会话库绝对路径；
/// - `XIDONG_NOTIFY` 继承值非空且 ≠ `XIDONG_SESSION` 时保留，否则默认本会话库；
/// - `XIDONG_PARENT` 优先保留继承的 `XIDONG_PARENT`（非空且 ≠ `XIDONG_SESSION`，中间层），
///   否则取继承的 `XIDONG_SESSION`（非空且 ≠ 本会话库，孵化者），都没有则不输出；
/// - `XIDONG_NOTIFY_CALL_ID` 仅在 `call_id` 为 `Some` 且非空时输出（不做格式校验）。
///
/// 不处理 `XIDONG_INBOX_PHASE`：该变量的相位由各调用点自行决定。
pub fn xidong_env(
    self_db: &Path,
    incoming: &impl Fn(&str) -> Option<String>,
    call_id: Option<&str>,
) -> BTreeMap<String, String> {
    let self_session = absolute_string(self_db);
    let incoming_session = incoming("XIDONG_SESSION").filter(|value| !value.is_empty());
    let incoming_notify = incoming("XIDONG_NOTIFY").filter(|value| !value.is_empty());
    let incoming_parent = incoming("XIDONG_PARENT").filter(|value| !value.is_empty());

    let mut env = BTreeMap::new();
    env.insert("XIDONG_SESSION".to_string(), self_session.clone());

    // `XIDONG_NOTIFY`：继承值非空且指向别的会话时保留（子智能体回父），否则默认自己。
    let notify = match &incoming_notify {
        Some(value) if *value != self_session => value.clone(),
        _ => self_session.clone(),
    };
    env.insert("XIDONG_NOTIFY".to_string(), notify);

    // `XIDONG_PARENT`：中间层优先保留继承的祖父；否则把继承的 SESSION 记为孵化者。
    let parent = match &incoming_parent {
        Some(value) if *value != self_session => Some(value.clone()),
        _ => match &incoming_session {
            Some(value) if *value != self_session => Some(value.clone()),
            _ => None,
        },
    };
    if let Some(parent) = parent {
        env.insert("XIDONG_PARENT".to_string(), parent);
    }

    if let Some(call_id) = call_id.filter(|value| !value.is_empty()) {
        env.insert("XIDONG_NOTIFY_CALL_ID".to_string(), call_id.to_string());
    }

    env
}

/// 路径的绝对化字符串；绝对化失败时退回原路径。
fn absolute_string(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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

        // 无显式参数时用 XIDONG_SESSION；显式路径不算 cwd 规范化回退，故不告警。
        let resolved = resolve_target(&cwd, None, Some(from_env.to_str().unwrap()), &data).unwrap();
        assert_eq!(resolved.session.db, from_env);
        assert!(!resolved.warned);
        assert_eq!(resolved.cwd_absolute, cwd);

        // 相对 `XIDONG_SESSION` 与 notify 路径一致：按传入 cwd 绝对化。
        let resolved = resolve_target(&cwd, None, Some("env/from-env.db"), &data).unwrap();
        assert_eq!(resolved.session.db, cwd.join("env").join("from-env.db"));
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

    /// 由键值对构造 env getter；缺失键返回 `None`。
    fn env_from(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    const SELF: &str = "/data/self/session.db";

    #[test]
    fn explicit_session_outputs_dir_nests_artifact_name() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let data = dir.path().join("data");
        let root = dir.path().join("custom");
        let a = resolve(&cwd, Some(&root.join("a.db")), &data).unwrap();
        let b = resolve(&cwd, Some(&root.join("b.db")), &data).unwrap();

        assert_eq!(outputs_dir(&a.session), root.join("outputs").join("a.db"));
        assert_eq!(outputs_dir(&b.session), root.join("outputs").join("b.db"));
        assert_ne!(outputs_dir(&a.session), outputs_dir(&b.session));

        // 默认会话路径保持不变。
        let default = resolve(&cwd, None, &data).unwrap();
        assert_eq!(
            outputs_dir(&default.session),
            default.session.root.join("outputs")
        );
    }

    #[test]
    fn relative_notify_env_resolves_against_supplied_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = cwd.join("nested").join("notify.db");
        make_session(&target);

        // `XIDONG_NOTIFY` 是相对值：必须按传入的 cwd（而非进程当前目录）解析。
        let env = |key: &str| (key == "XIDONG_NOTIFY").then(|| "nested/notify.db".to_string());
        let resolved = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(resolved.resolved.session.db, target);
        assert!(!resolved.is_self);
    }

    #[test]
    fn absolute_notify_env_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = dir.path().join("absolute.db");
        make_session(&target);

        let value = target.to_string_lossy().into_owned();
        let env = |key: &str| (key == "XIDONG_NOTIFY").then(|| value.clone());
        let resolved = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(resolved.resolved.session.db, target);
    }

    #[test]
    fn relative_notify_equal_to_relative_session_falls_back_to_self() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = cwd.join("self.db");
        make_session(&target);

        let env = |key: &str| {
            matches!(key, "XIDONG_NOTIFY" | "XIDONG_SESSION").then(|| "self.db".to_string())
        };
        let resolved = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(resolved.resolved.session.db, target);
        assert!(resolved.is_self, "NOTIFY 与 SESSION 指向同一相对会话");
    }

    #[test]
    fn relative_to_resolves_against_supplied_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = cwd.join("target.db");
        make_session(&target);

        let resolved =
            resolve_notify_target(&cwd, Some(Path::new("target.db")), None, &data, &no_env)
                .unwrap();
        assert_eq!(resolved.resolved.session.db, target);
        assert!(resolved.explicit);
    }

    #[test]
    fn xidong_env_without_inheritance_defaults_to_self() {
        let env = xidong_env(Path::new(SELF), &env_from(&[]), None);
        assert_eq!(env["XIDONG_SESSION"], SELF);
        assert_eq!(env["XIDONG_NOTIFY"], SELF);
        assert!(
            !env.contains_key("XIDONG_PARENT"),
            "无继承不应有 XIDONG_PARENT"
        );
        assert!(
            !env.contains_key("XIDONG_NOTIFY_CALL_ID"),
            "未传 call_id 不应注入"
        );
    }

    #[test]
    fn xidong_env_sets_parent_from_inherited_session() {
        let incoming = env_from(&[("XIDONG_SESSION", "/data/parent/session.db")]);
        let env = xidong_env(Path::new(SELF), &incoming, None);
        assert_eq!(
            env["XIDONG_SESSION"], SELF,
            "XIDONG_SESSION 无条件重设为自己"
        );
        assert_eq!(env["XIDONG_NOTIFY"], SELF, "未继承 NOTIFY 时默认自己");
        assert_eq!(env["XIDONG_PARENT"], "/data/parent/session.db");
    }

    #[test]
    fn xidong_env_keeps_foreign_inherited_notify() {
        let incoming = env_from(&[("XIDONG_NOTIFY", "/data/other/session.db")]);
        let env = xidong_env(Path::new(SELF), &incoming, None);
        assert_eq!(env["XIDONG_NOTIFY"], "/data/other/session.db");
        assert!(!env.contains_key("XIDONG_PARENT"));
    }

    #[test]
    fn xidong_env_self_notify_falls_back_to_self() {
        let incoming = env_from(&[("XIDONG_NOTIFY", SELF)]);
        let env = xidong_env(Path::new(SELF), &incoming, None);
        assert_eq!(env["XIDONG_NOTIFY"], SELF);
    }

    #[test]
    fn xidong_env_middle_layer_keeps_grandparent() {
        let incoming = env_from(&[
            ("XIDONG_PARENT", "/data/grand/session.db"),
            ("XIDONG_SESSION", "/data/parent/session.db"),
        ]);
        let env = xidong_env(Path::new(SELF), &incoming, None);
        assert_eq!(
            env["XIDONG_PARENT"], "/data/grand/session.db",
            "中间层应保留继承的祖父而非孵化者"
        );
    }

    #[test]
    fn xidong_env_call_id_only_when_present_and_non_empty() {
        let env = xidong_env(Path::new(SELF), &env_from(&[]), Some("call_abc"));
        assert_eq!(env["XIDONG_NOTIFY_CALL_ID"], "call_abc");

        let env = xidong_env(Path::new(SELF), &env_from(&[]), Some(""));
        assert!(
            !env.contains_key("XIDONG_NOTIFY_CALL_ID"),
            "空 call_id 不注入"
        );

        let env = xidong_env(Path::new(SELF), &env_from(&[]), None);
        assert!(!env.contains_key("XIDONG_NOTIFY_CALL_ID"));
    }

    #[test]
    fn xidong_env_treats_empty_inherited_values_as_missing() {
        let incoming = env_from(&[
            ("XIDONG_SESSION", ""),
            ("XIDONG_NOTIFY", ""),
            ("XIDONG_PARENT", ""),
        ]);
        let env = xidong_env(Path::new(SELF), &incoming, Some(""));
        assert_eq!(env["XIDONG_SESSION"], SELF);
        assert_eq!(env["XIDONG_NOTIFY"], SELF);
        assert!(!env.contains_key("XIDONG_PARENT"));
        assert!(!env.contains_key("XIDONG_NOTIFY_CALL_ID"));
    }

    /// 创建一个已初始化的 xidong 会话库（父目录不存在时一并创建）。
    fn make_session(db: &Path) {
        let session = crate::store::Session::new(db.to_path_buf());
        crate::store::append(&session, &[crate::msg::Record::user("seed")]).unwrap();
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// 优先级矩阵：`--to` > `--session` > `XIDONG_NOTIFY` > `XIDONG_SESSION` > cwd 默认。
    #[test]
    fn resolve_notify_target_prefers_to_then_session_then_env_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let to_db = dir.path().join("to").join("to.db");
        let session_db = dir.path().join("session.db");
        let notify_db = dir.path().join("notify.db");
        let env_db = dir.path().join("env.db");
        make_session(&to_db);
        make_session(&session_db);
        make_session(&notify_db);
        make_session(&env_db);

        // `--to` 最高优先（与 `--session` 同时给出且不同会先报用法错误，见下个用例），
        // 且标记 explicit、视为跨会话。
        let target = resolve_notify_target(&cwd, Some(&to_db), None, &data, &no_env).unwrap();
        assert_eq!(target.resolved.session.db, to_db);
        assert!(target.explicit);
        assert!(!target.is_self);

        // 无 `--to` 时 `--session` 优先。
        let target = resolve_notify_target(&cwd, None, Some(&session_db), &data, &no_env).unwrap();
        assert_eq!(target.resolved.session.db, session_db);
        assert!(!target.explicit);

        // `XIDONG_NOTIFY` 非空且 ≠ `XIDONG_SESSION` 时优先于 `XIDONG_SESSION`，且视为跨会话。
        let env = |key: &str| match key {
            "XIDONG_NOTIFY" => Some(notify_db.to_string_lossy().into_owned()),
            "XIDONG_SESSION" => Some(env_db.to_string_lossy().into_owned()),
            _ => None,
        };
        let target = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(target.resolved.session.db, notify_db);
        assert!(!target.is_self);

        // `XIDONG_NOTIFY == XIDONG_SESSION` 时回退到 `XIDONG_SESSION`。
        let same = notify_db.to_string_lossy().into_owned();
        let env =
            |key: &str| matches!(key, "XIDONG_NOTIFY" | "XIDONG_SESSION").then(|| same.clone());
        let target = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(target.resolved.session.db, notify_db);

        // 只有 `XIDONG_SESSION`。
        let env =
            |key: &str| (key == "XIDONG_SESSION").then(|| env_db.to_string_lossy().into_owned());
        let target = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(target.resolved.session.db, env_db);
        assert!(target.is_self, "XIDONG_SESSION 指向的就是自身会话");

        // `XIDONG_NOTIFY` 为空串视为未设置，回落到 `XIDONG_SESSION`。
        let env = |key: &str| match key {
            "XIDONG_NOTIFY" => Some(String::new()),
            "XIDONG_SESSION" => Some(env_db.to_string_lossy().into_owned()),
            _ => None,
        };
        let target = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert_eq!(target.resolved.session.db, env_db);

        // 环境变量都为空串时回退 cwd 默认会话。
        let env = |key: &str| match key {
            "XIDONG_NOTIFY" | "XIDONG_SESSION" => Some(String::new()),
            _ => None,
        };
        let target = resolve_notify_target(&cwd, None, None, &data, &env).unwrap();
        assert!(target.resolved.session.root.starts_with(&data));
        assert!(target.is_self);

        // 都无：cwd 默认会话，且视为自身。
        let target = resolve_notify_target(&cwd, None, None, &data, &no_env).unwrap();
        assert!(target.resolved.session.root.starts_with(&data));
        assert!(target.is_self, "无继承时默认目标是自身");
    }

    #[test]
    fn resolve_notify_target_rejects_conflicting_to_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        make_session(&a);
        make_session(&b);

        let err = resolve_notify_target(&cwd, Some(&a), Some(&b), &data, &no_env).unwrap_err();
        assert!(matches!(err, NotifyTargetError::Usage(_)), "{err:?}");

        // 同一数据库路径允许，仍按显式处理。
        let target = resolve_notify_target(&cwd, Some(&a), Some(&a), &data, &no_env).unwrap();
        assert!(target.explicit);
        assert_eq!(target.resolved.session.db, a);
    }

    #[test]
    fn resolve_notify_target_rejects_empty_to() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_notify_target(
            &dir.path().join("cwd"),
            Some(Path::new("")),
            None,
            &dir.path().join("data"),
            &no_env,
        )
        .unwrap_err();
        assert!(matches!(err, NotifyTargetError::Usage(_)), "{err:?}");
    }

    #[test]
    fn resolve_notify_target_strict_rejects_missing_without_creating() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let missing = dir.path().join("absent").join("session.db");

        let err = resolve_notify_target(&cwd, Some(&missing), None, &data, &no_env).unwrap_err();
        assert!(format!("{err:#}").contains("目标会话不存在"), "{err:#}");
        assert!(
            !missing.parent().unwrap().exists(),
            "严格校验不得创建目标目录"
        );
    }

    #[test]
    fn resolve_notify_target_strict_rejects_directory_without_session_db() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let empty_dir = dir.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let err = resolve_notify_target(&cwd, Some(&empty_dir), None, &data, &no_env).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("目标会话不存在"), "{message}");
        assert!(message.contains("session.db"), "应指明探测路径：{message}");
        assert!(!empty_dir.join("session.db").exists());
    }

    #[test]
    fn resolve_notify_target_strict_rejects_legacy_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let legacy = dir.path().join("old.jsonl");
        std::fs::write(&legacy, b"{}\n").unwrap();

        let err = resolve_notify_target(&cwd, Some(&legacy), None, &data, &no_env).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("旧版 JSONL"), "{message}");
        assert!(message.contains("--clear"), "{message}");
    }

    #[test]
    fn resolve_notify_target_strict_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        // 0 字节文件不是已初始化的 xidong 库：store 会把它当作空会话，显式 `--to` 必须拒绝，
        // 否则发布时会在目标处新建 schema（违反「绝不创建目标文件」）。
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        let err = resolve_notify_target(&cwd, Some(&empty), None, &data, &no_env).unwrap_err();
        assert!(
            format!("{err:#}").contains("不是 xidong 会话数据库"),
            "0 字节文件应被拒绝：{err:#}"
        );
    }

    #[test]
    fn resolve_notify_target_strict_rejects_archived_session() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let archived = dir.path().join("archived.db");
        make_session(&archived);
        crate::store::set_archived(&crate::store::Session::new(archived.clone())).unwrap();

        let err = resolve_notify_target(&cwd, Some(&archived), None, &data, &no_env).unwrap_err();
        assert!(format!("{err:#}").contains("已归档"), "{err:#}");
    }

    #[test]
    fn same_session_path_normalizes_dotdot_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let db = real.join("session.db");
        make_session(&db);
        let sub = real.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        assert!(same_session_path(&sub.join("..").join("session.db"), &db));
        #[cfg(unix)]
        {
            let link = dir.path().join("link.db");
            std::os::unix::fs::symlink(&db, &link).unwrap();
            assert!(same_session_path(&link, &db), "符号链接应解析为同一会话");
        }

        // 不存在的路径退化为词法归一化：去掉 `.` 与 `..`。
        let missing = dir.path().join("a").join("..").join("b.db");
        assert!(same_session_path(&missing, &dir.path().join("b.db")));
    }

    #[test]
    fn explicit_target_session_canonicalizes_symlink_db() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let db = real.join("session.db");
        make_session(&db);

        #[cfg(unix)]
        {
            let link = dir.path().join("link.db");
            std::os::unix::fs::symlink(&db, &link).unwrap();
            let session = explicit_target_session(&link).unwrap();
            assert_eq!(session.db, db.canonicalize().unwrap());
            assert_eq!(session.root, real.canonicalize().unwrap());
        }
    }
}
