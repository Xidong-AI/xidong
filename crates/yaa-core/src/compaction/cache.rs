//! 摘要侧车缓存（D50/D51）：`<会话根>/summaries/<id>.json`。
//!
//! 摘要不进 `default.jsonl`：链式递进由 `previous` 表达，`covers` 列出被遮蔽的
//! 记录 id（含前驱覆盖的记录）。写入用「同目录临时文件 + rename」保证原子，坏
//! 文件在读取时跳过而不让整份缓存不可用。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 一条已落盘的摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryEntry {
    /// 条目 id：`sha256(model ⧺ covers)` 前 32 位十六进制，亦为文件名（去 `.json`）。
    pub id: String,
    /// 生成该摘要的模型名。
    pub model: String,
    /// 前驱摘要 id（链式递进的上一环）；无前驱为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// 被遮蔽的记录 id，按物理顺序（含前驱覆盖的记录）。
    pub covers: Vec<String>,
    /// 摘要正文。
    pub summary: String,
    /// 生成时间（Unix 毫秒）。
    pub created_ts: i64,
    /// 摘要自身的 token 估算（可选，供状态展示）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<usize>,
}

/// 摘要缓存目录句柄。
#[derive(Debug, Clone)]
pub struct SummaryCache {
    dir: PathBuf,
}

impl SummaryCache {
    /// 以会话根与 artifact 名构造；目录为 `<session_root>/summaries[/<artifact_name>]`
    /// （未必存在）。默认会话的 artifact 名为空串，路径保持 `<session_root>/summaries`。
    pub fn new(session_root: &Path, artifact_name: &str) -> Self {
        let mut dir = session_root.join("summaries");
        if !artifact_name.is_empty() {
            dir.push(artifact_name);
        }
        Self { dir }
    }

    /// 缓存目录路径。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 由模型名与遮蔽记录 id 序列计算条目 id（sha256 前 32 位十六进制）。
    ///
    /// 每段以 `\0` 分隔，避免 `["ab","c"]` 与 `["a","bc"]` 撞键。
    pub fn key(model: &str, covers: &[String]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(model.as_bytes());
        hasher.update([0u8]);
        for id in covers {
            hasher.update(id.as_bytes());
            hasher.update([0u8]);
        }
        let digest = format!("{:x}", hasher.finalize());
        digest[..32].to_string()
    }

    /// 条目文件路径：`<dir>/<id>.json`。
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// 读取链尾条目（`created_ts` 最大者，并列取 id 大者）。
    ///
    /// 目录缺失视为无缓存；无法解析的文件跳过（不因单条坏文件放弃整份缓存）。
    pub fn load_latest(&self) -> anyhow::Result<Option<SummaryEntry>> {
        let dir = match fs::read_dir(&self.dir) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let mut latest: Option<SummaryEntry> = None;
        for item in dir {
            let path = item?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<SummaryEntry>(&text) else {
                continue;
            };
            let replace = match &latest {
                Some(current) => (entry.created_ts, &entry.id) > (current.created_ts, &current.id),
                None => true,
            };
            if replace {
                latest = Some(entry);
            }
        }
        Ok(latest)
    }

    /// 原子写入条目（同目录临时文件 + rename），返回写入路径。
    pub fn store(&self, entry: &SummaryEntry) -> anyhow::Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&entry.id);
        let tmp = self.dir.join(format!(".{}.tmp", entry.id));
        let text = serde_json::to_string_pretty(entry)?;
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, ts: i64, previous: Option<&str>) -> SummaryEntry {
        SummaryEntry {
            id: id.to_string(),
            model: "m".to_string(),
            previous: previous.map(str::to_string),
            covers: vec!["a".to_string(), "b".to_string()],
            summary: "摘要".to_string(),
            created_ts: ts,
            tokens: Some(3),
        }
    }

    #[test]
    fn key_is_stable_and_field_separated() {
        let a = SummaryCache::key("m", &["ab".to_string(), "c".to_string()]);
        let b = SummaryCache::key("m", &["a".to_string(), "bc".to_string()]);
        assert_ne!(a, b, "不同划分不得撞键");
        assert_eq!(a.len(), 32);
        assert_eq!(
            a,
            SummaryCache::key("m", &["ab".to_string(), "c".to_string()])
        );
        assert_ne!(
            a,
            SummaryCache::key("other", &["ab".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn store_then_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SummaryCache::new(dir.path(), "");
        let first = entry("id1", 10, None);
        cache.store(&first).unwrap();

        let loaded = cache.load_latest().unwrap().unwrap();
        assert_eq!(loaded, first);
        assert_eq!(
            cache.path_for("id1"),
            dir.path().join("summaries").join("id1.json")
        );
    }

    #[test]
    fn load_latest_picks_newest_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SummaryCache::new(dir.path(), "");
        cache.store(&entry("old", 10, None)).unwrap();
        cache.store(&entry("new", 20, Some("old"))).unwrap();

        let loaded = cache.load_latest().unwrap().unwrap();
        assert_eq!(loaded.id, "new");
        assert_eq!(loaded.previous.as_deref(), Some("old"));
    }

    #[test]
    fn missing_dir_is_none_and_bad_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SummaryCache::new(dir.path(), "");
        assert!(
            cache.load_latest().unwrap().is_none(),
            "目录缺失应视为无缓存"
        );

        cache.store(&entry("good", 10, None)).unwrap();
        fs::write(cache.dir().join("broken.json"), "not json").unwrap();
        let loaded = cache.load_latest().unwrap().unwrap();
        assert_eq!(loaded.id, "good", "坏文件应被跳过而非让缓存整体失败");
    }

    #[test]
    fn artifact_name_nests_summaries_dir() {
        let dir = tempfile::tempdir().unwrap();
        // 默认会话（空串）保持既有目录。
        assert_eq!(
            SummaryCache::new(dir.path(), "").dir(),
            dir.path().join("summaries")
        );
        // 显式会话按 artifact 名嵌套，互不混放。
        assert_eq!(
            SummaryCache::new(dir.path(), "a.db").dir(),
            dir.path().join("summaries").join("a.db")
        );
        assert_ne!(
            SummaryCache::new(dir.path(), "a.db").dir(),
            SummaryCache::new(dir.path(), "b.db").dir()
        );
    }

    #[test]
    fn store_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SummaryCache::new(dir.path(), "");
        cache.store(&entry("id1", 10, None)).unwrap();
        let leftovers: Vec<_> = fs::read_dir(cache.dir())
            .unwrap()
            .filter_map(|item| item.ok())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件不应残留：{leftovers:?}");
    }
}
