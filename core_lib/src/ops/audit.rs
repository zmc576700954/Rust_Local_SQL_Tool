//! 追加式 JSONL 审计日志。写入失败必须让调用方中止（fail closed）。

use std::io::BufRead;
use std::path::PathBuf;

use crate::ops::error::OpsError;
use crate::ops::types::AuditRecord;

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: Option<PathBuf>,
}

impl AuditLog {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    pub fn append(&self, record: &AuditRecord) -> Result<(), OpsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(record)?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }

    /// 读取最近 N 条，返回按时间倒序。
    pub fn recent(&self, limit: usize) -> Result<Vec<AuditRecord>, OpsError> {
        let Some(path) = &self.path else {
            return Ok(vec![]);
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => return Err(err.into()),
        };
        let reader = std::io::BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines().flatten() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<AuditRecord>(&line) {
                records.push(record);
            }
        }
        records.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
        records.truncate(limit);
        Ok(records)
    }
}
