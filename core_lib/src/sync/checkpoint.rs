//! 数据同步断点续跑的状态文件。
//!
//! 约定：
//! - 每个作业一个文件：`<dir>/<job_key>.json`；
//! - 先写临时文件再 `rename`，避免进程中断留下半截 JSON；
//! - `job_key` 是源/目标/模式/批次/表结构的指纹，续跑时指纹不一致直接拒绝，
//!   避免拿旧断点去跑一份已经变化的任务。

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CHECKPOINT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableCheckpoint {
    /// 已提交批次的最后一组主键值（按主键列顺序，文本形式）。
    pub last_pk: Vec<String>,
    /// 该表累计已写入的行数。
    pub rows_copied: u64,
    /// 整表是否已经处理完（含 mirror 尾部删除）。
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    pub version: u32,
    pub job_key: String,
    pub tables: BTreeMap<String, TableCheckpoint>,
}

impl Checkpoint {
    pub fn new(job_key: impl Into<String>) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            job_key: job_key.into(),
            tables: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, job_key: &str) -> PathBuf {
        self.dir.join(format!("{job_key}.json"))
    }

    /// 读取断点。文件不存在返回 `None`；解析失败或指纹不一致必须显式报错。
    pub fn load(&self, job_key: &str) -> Result<Option<Checkpoint>, AppError> {
        let path = self.path_for(job_key);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).map_err(|e| {
            AppError::InternalError(format!("读取同步断点失败 {}: {e}", path.display()))
        })?;
        let checkpoint: Checkpoint = serde_json::from_str(&text).map_err(|e| {
            AppError::BadRequest(format!(
                "同步断点文件损坏，拒绝续跑 {}: {e}",
                path.display()
            ))
        })?;
        if checkpoint.version != CHECKPOINT_VERSION {
            return Err(AppError::BadRequest(format!(
                "同步断点版本不匹配（文件 {}，当前 {}），拒绝续跑",
                checkpoint.version, CHECKPOINT_VERSION
            )));
        }
        if checkpoint.job_key != job_key {
            return Err(AppError::BadRequest(format!(
                "同步断点指纹不一致（文件 {}，当前 {}），拒绝续跑",
                checkpoint.job_key, job_key
            )));
        }
        Ok(Some(checkpoint))
    }

    /// 原子写入：临时文件落盘后 rename 覆盖。
    pub fn save(&self, checkpoint: &Checkpoint) -> Result<(), AppError> {
        fs::create_dir_all(&self.dir).map_err(|e| {
            AppError::InternalError(format!(
                "创建同步断点目录失败 {}: {e}",
                self.dir.display()
            ))
        })?;
        let path = self.path_for(&checkpoint.job_key);
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(checkpoint)
            .map_err(|e| AppError::InternalError(format!("序列化同步断点失败: {e}")))?;
        {
            let mut f = fs::File::create(&tmp).map_err(|e| {
                AppError::InternalError(format!(
                    "写入同步断点临时文件失败 {}: {e}",
                    tmp.display()
                ))
            })?;
            f.write_all(&bytes).map_err(|e| {
                AppError::InternalError(format!(
                    "写入同步断点临时文件失败 {}: {e}",
                    tmp.display()
                ))
            })?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path).map_err(|e| {
            AppError::InternalError(format!(
                "提交同步断点失败 {} -> {}: {e}",
                tmp.display(),
                path.display()
            ))
        })?;
        Ok(())
    }

    pub fn clear(&self, job_key: &str) -> Result<(), AppError> {
        let path = self.path_for(job_key);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                AppError::InternalError(format!(
                    "清理同步断点失败 {}: {e}",
                    path.display()
                ))
            })?;
        }
        Ok(())
    }
}

/// 拼接指纹输入：不同字段之间用不可见分隔符，避免边界歧义。
pub fn canonical_fingerprint_input(parts: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in parts {
        out.push_str(key);
        out.push('\u{1f}');
        out.push_str(value);
        out.push('\u{1e}');
    }
    out
}

pub fn fingerprint(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pg-data-sync-checkpoint-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_input() {
        let a = canonical_fingerprint_input(&[("source", "s1"), ("target", "t1")]);
        let b = canonical_fingerprint_input(&[("source", "s1"), ("target", "t1")]);
        let c = canonical_fingerprint_input(&[("source", "s1"), ("target", "t2")]);
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c));
        assert_eq!(fingerprint(&a).len(), 64);
    }

    #[test]
    fn save_and_load_roundtrip_is_atomic() {
        let dir = temp_dir("roundtrip");
        let store = CheckpointStore::new(&dir);
        let mut cp = Checkpoint::new("key-1");
        cp.tables.insert(
            "public.users".to_string(),
            TableCheckpoint {
                last_pk: vec!["42".to_string()],
                rows_copied: 42,
                completed: false,
            },
        );
        store.save(&cp).expect("save checkpoint");
        let loaded = store.load("key-1").expect("load checkpoint").expect("exists");
        assert_eq!(loaded, cp);
        // 不应留下临时文件
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_rejects_mismatched_fingerprint() {
        let dir = temp_dir("mismatch");
        fs::create_dir_all(&dir).unwrap();
        // 文件路径对应 new-key，但内部记录的指纹是 old-key。
        let bytes = serde_json::to_vec(&Checkpoint::new("old-key")).unwrap();
        fs::write(CheckpointStore::new(&dir).path_for("new-key"), bytes).unwrap();
        let store = CheckpointStore::new(&dir);
        let err = store
            .load("new-key")
            .expect_err("指纹不一致必须拒绝续跑");
        assert!(err.to_string().contains("指纹不一致"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = temp_dir("missing");
        let store = CheckpointStore::new(&dir);
        assert!(store.load("absent").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_file_errors_instead_of_silently_restarting() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.json"), "{not json").unwrap();
        let store = CheckpointStore::new(&dir);
        let err = store.load("bad").expect_err("损坏断点必须报错");
        assert!(err.to_string().contains("损坏"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
