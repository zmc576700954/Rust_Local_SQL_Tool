//! 追加式 JSONL 审计日志。写入失败必须让调用方中止（fail closed）。

use std::io::BufRead;
use std::path::PathBuf;

use crate::ops::error::OpsError;
use crate::ops::inspection::redact::{scrub_json, scrub_text};
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
        // 审计文件是取证材料，不能把连接串或口令原样写进去。
        let mut record = record.clone();
        scrub_json(&mut record.detail, &[]);
        if let Some(db_name) = record.db_name.as_mut() {
            let (clean, _) = scrub_text(db_name, &[]);
            *db_name = clean;
        }
        let line = serde_json::to_string(&record)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::redact::contains_sensitive_shape;
    use serde_json::json;

    #[test]
    fn audit_file_never_contains_connection_strings() {
        let path = std::env::temp_dir().join(format!("audit-redact-{}.jsonl", uuid::Uuid::new_v4()));
        let log = AuditLog::new(Some(path.clone()));
        let mut record = AuditRecord::new("inspection_alert_ack", "ok");
        record.db_name = Some("mysql://root:rootpass@127.0.0.1:3306/shop".into());
        record.detail = json!({
            "dsn": "mysql://root:rootpass@127.0.0.1:3306/shop",
            "webhook": "https://hooks.example.com/a?password=plain123",
            "note": "库 shop 的表 orders 索引 idx_created 异常",
        });
        log.append(&record).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!contains_sensitive_shape(&content), "audit leaked: {content}");
        // 只删除凭据本体；主机、端口、库名等排查信息保留。
        for shape in ["rootpass", "plain123", "password="] {
            assert!(!content.contains(shape), "audit leaked {shape}: {content}");
        }
        assert!(content.contains("mysql://root@127.0.0.1:3306/shop"), "{content}");
        assert!(content.contains("shop"));
        assert!(content.contains("orders"));
        assert!(content.contains("idx_created"));
    }
}
