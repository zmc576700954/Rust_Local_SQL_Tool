//! 巡检敏感信息清洗。
//!
//! 两条防线：
//! 1. 源头：巡检快照落库前清洗 message / evidence / skipped_reason，避免连接串进入历史记录；
//! 2. 展示与通知：历史数据返回界面、写审计、发邮件/Webhook 前再次强制清洗。
//!
//! 清洗是“删除”而不是“替换成 ***”，避免保留 `scheme://`、`user:pass@`、
//! `password=`、`@host:port` 这些可辨认的结构。

use serde_json::Value;

use crate::ops::inspection::types::{
    AlertRecord, CheckResult, EvidenceItem, InspectionRunResult, InspectionSnapshot, SnapshotDiff,
};

fn is_delimiter(value: char) -> bool {
    value.is_whitespace()
        || matches!(
            value,
            '"' | '\''
                | '<'
                | '>'
                | ','
                | ';'
                | ')'
                | '('
                | ']'
                | '['
                | '}'
                | '{'
                | '，'
                | '。'
                | '；'
                | '）'
                | '（'
                | '、'
        )
}

fn is_scheme_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '+' | '-' | '.')
}

fn is_host_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_')
}

const SECRET_KEYS: [&str; 6] = ["password", "passwd", "pwd", "token", "secret", "authorization"];

/// 扫描并删除形如 `scheme://...` 的整段 URI。
fn remove_uri_tokens(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index + 2 < chars.len() {
        if chars[index] == ':' && chars[index + 1] == '/' && chars[index + 2] == '/' {
            let mut start = index;
            while start > 0 && is_scheme_char(chars[start - 1]) {
                start -= 1;
            }
            if start < index {
                let mut end = index + 3;
                while end < chars.len() && !is_delimiter(chars[end]) {
                    end += 1;
                }
                let mut next = String::new();
                next.extend(&chars[..start]);
                next.extend(&chars[end..]);
                return remove_uri_tokens(&next);
            }
        }
        index += 1;
    }
    input.to_string()
}

/// 删除 `user:pass@` 所在的整个 token（包含其后的主机与路径）。
fn remove_userinfo(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' {
            let mut start = index;
            while start > 0 && !is_delimiter(chars[start - 1]) {
                start -= 1;
            }
            let prefix = &chars[start..index];
            if prefix.contains(&':') && !prefix.contains(&'@') {
                let mut end = index + 1;
                while end < chars.len() && !is_delimiter(chars[end]) {
                    end += 1;
                }
                let mut next = String::new();
                next.extend(&chars[..start]);
                next.extend(&chars[end..]);
                return remove_userinfo(&next);
            }
        }
        index += 1;
    }
    input.to_string()
}

/// 删除 `@host:port` 所在的整个 token。
fn remove_at_host_port(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' {
            let mut host_end = index + 1;
            while host_end < chars.len() && is_host_char(chars[host_end]) {
                host_end += 1;
            }
            let has_host = host_end > index + 1;
            let has_port = host_end < chars.len()
                && chars[host_end] == ':'
                && host_end + 1 < chars.len()
                && chars[host_end + 1].is_ascii_digit();
            if has_host && has_port {
                let mut start = index;
                while start > 0 && !is_delimiter(chars[start - 1]) {
                    start -= 1;
                }
                let mut end = host_end + 1;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                while end < chars.len() && !is_delimiter(chars[end]) {
                    end += 1;
                }
                let mut next = String::new();
                next.extend(&chars[..start]);
                next.extend(&chars[end..]);
                return remove_at_host_port(&next);
            }
        }
        index += 1;
    }
    input.to_string()
}

/// 删除 `password=...` / `token: ...` 这类键值片段（连同键名）。
fn remove_secret_assignments(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let boundary = index == 0 || is_delimiter(chars[index - 1]) || chars[index - 1] == '&';
        if boundary {
            let rest: String = chars[index..]
                .iter()
                .take(24)
                .collect::<String>()
                .to_ascii_lowercase();
            let mut matched_key = None;
            for key in SECRET_KEYS {
                if rest.starts_with(key) {
                    matched_key = Some((key, key.len()));
                    break;
                }
            }
            if let Some((key, key_len)) = matched_key {
                let after_key = index + key_len;
                let after_boundary = after_key == chars.len()
                    || is_delimiter(chars[after_key])
                    || matches!(chars[after_key], '=' | ':');
                if after_boundary {
                    let mut cursor = after_key;
                    while cursor < chars.len() && chars[cursor] == ' ' {
                        cursor += 1;
                    }
                    if cursor < chars.len() && (chars[cursor] == '=' || chars[cursor] == ':') {
                        let mut end = cursor + 1;
                        while end < chars.len() && chars[end] == ' ' {
                            end += 1;
                        }
                        if key == "authorization" {
                            while end < chars.len() && chars[end] != '\n' {
                                end += 1;
                            }
                        } else {
                            while end < chars.len() && !is_delimiter(chars[end]) && chars[end] != '&'
                            {
                                end += 1;
                            }
                        }
                        let mut next = String::new();
                        next.extend(&chars[..index]);
                        next.extend(&chars[end..]);
                        return remove_secret_assignments(&next);
                    }
                }
            }
        }
        index += 1;
    }
    input.to_string()
}

/// 强制清洗文本。返回 `(清洗后的文本, 是否发生变化)`。
pub fn scrub_text(input: &str, secrets: &[String]) -> (String, bool) {
    let mut output = input.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            output = output.replace(secret.as_str(), "");
        }
    }
    output = remove_uri_tokens(&output);
    output = remove_userinfo(&output);
    output = remove_at_host_port(&output);
    output = remove_secret_assignments(&output);
    let changed = output != input;
    (output.trim().to_string(), changed)
}

/// 递归清洗 JSON 中的字符串值；用于证据字段与审计 detail。
pub fn scrub_json(value: &mut Value, secrets: &[String]) -> bool {
    match value {
        Value::String(text) => {
            let (clean, changed) = scrub_text(text, secrets);
            if changed {
                *text = clean;
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| scrub_json(item, secrets) || changed),
        Value::Object(map) => {
            let mut changed = false;
            for item in map.values_mut() {
                changed = scrub_json(item, secrets) || changed;
            }
            changed
        }
        _ => false,
    }
}

pub fn scrub_evidence(item: &mut EvidenceItem, secrets: &[String]) -> bool {
    let (key, mut changed) = scrub_text(&item.key, secrets);
    item.key = key;
    let (message, message_changed) = scrub_text(&item.message, secrets);
    item.message = message;
    changed = changed || message_changed;
    changed = scrub_json(&mut item.fields, secrets) || changed;
    changed
}

pub fn scrub_check(check: &mut CheckResult, secrets: &[String]) -> bool {
    let mut changed = false;
    let (title, title_changed) = scrub_text(&check.title, secrets);
    check.title = title;
    changed |= title_changed;
    let (summary, summary_changed) = scrub_text(&check.summary, secrets);
    check.summary = summary;
    changed |= summary_changed;
    if let Some(reason) = check.skipped_reason.as_deref() {
        let (clean, reason_changed) = scrub_text(reason, secrets);
        check.skipped_reason = Some(clean);
        changed |= reason_changed;
    }
    for item in &mut check.evidence {
        changed = scrub_evidence(item, secrets) || changed;
    }
    changed
}

pub fn scrub_snapshot(snapshot: &mut InspectionSnapshot, secrets: &[String]) -> bool {
    let (db_name, mut changed) = scrub_text(&snapshot.db_name, secrets);
    snapshot.db_name = db_name;
    for check in &mut snapshot.checks {
        changed = scrub_check(check, secrets) || changed;
    }
    changed
}

pub fn scrub_alert(alert: &mut AlertRecord, secrets: &[String]) -> bool {
    let (db_name, mut changed) = scrub_text(&alert.db_name, secrets);
    alert.db_name = db_name;
    let (title, title_changed) = scrub_text(&alert.title, secrets);
    alert.title = title;
    let (message, message_changed) = scrub_text(&alert.message, secrets);
    alert.message = message;
    let (key, key_changed) = scrub_text(&alert.evidence_key, secrets);
    alert.evidence_key = key;
    changed = changed || title_changed || message_changed || key_changed;
    for delivery in &mut alert.deliveries {
        if let Some(error) = delivery.last_error.as_deref() {
            let (clean, error_changed) = scrub_text(error, secrets);
            delivery.last_error = Some(clean);
            changed = changed || error_changed;
        }
    }
    changed
}

pub fn scrub_diff(diff: &mut SnapshotDiff, secrets: &[String]) -> bool {
    let mut changed = false;
    for item in diff.added.iter_mut().chain(diff.resolved.iter_mut()) {
        let (key, key_changed) = scrub_text(&item.evidence_key, secrets);
        item.evidence_key = key;
        let (message, message_changed) = scrub_text(&item.message, secrets);
        item.message = message;
        changed = changed || key_changed || message_changed;
        changed = scrub_json(&mut item.fields, secrets) || changed;
    }
    for item in &mut diff.changed {
        for evidence in [&mut item.before, &mut item.after] {
            let (key, key_changed) = scrub_text(&evidence.evidence_key, secrets);
            evidence.evidence_key = key;
            let (message, message_changed) = scrub_text(&evidence.message, secrets);
            evidence.message = message;
            changed = changed || key_changed || message_changed;
            changed = scrub_json(&mut evidence.fields, secrets) || changed;
        }
    }
    changed
}

pub fn scrub_run_result(result: &mut InspectionRunResult, secrets: &[String]) -> bool {
    let mut changed = scrub_snapshot(&mut result.snapshot, secrets);
    changed = scrub_diff(&mut result.diff, secrets) || changed;
    for alert in &mut result.alerts {
        changed = scrub_alert(alert, secrets) || changed;
    }
    for event in &mut result.events {
        changed = scrub_alert(&mut event.alert, secrets) || changed;
    }
    changed
}

/// 是否仍包含需要删除的敏感形态；用于测试与运行期断言。
pub fn contains_sensitive_shape(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("://") {
        return true;
    }
    if lower.contains("password=")
        || lower.contains("passwd=")
        || lower.contains("pwd=")
        || lower.contains("password:")
        || lower.contains("passwd:")
        || lower.contains("pwd:")
    {
        return true;
    }
    let chars: Vec<char> = text.chars().collect();
    for index in 0..chars.len() {
        if chars[index] != '@' {
            continue;
        }
        let mut host_end = index + 1;
        while host_end < chars.len() && is_host_char(chars[host_end]) {
            host_end += 1;
        }
        if host_end > index + 1
            && host_end < chars.len()
            && chars[host_end] == ':'
            && host_end + 1 < chars.len()
            && chars[host_end + 1].is_ascii_digit()
        {
            return true;
        }
        let mut start = index;
        while start > 0 && !is_delimiter(chars[start - 1]) {
            start -= 1;
        }
        if chars[start..index].contains(&':') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::{
        CheckStatus, DiffEvidence, EvidenceItem, InspectionSnapshot, SnapshotStatus,
    };
    use crate::ops::types::Severity;
    use serde_json::json;

    fn secrets() -> Vec<String> {
        vec!["plain-secret-123".into(), "hook-token-456".into()]
    }

    #[test]
    fn removes_dsn_and_keeps_db_and_object_names() {
        let raw = "库 shop 的表 orders 索引 idx_created 未使用；连接 mysql://root:rootpass@127.0.0.1:3306/shop?ssl=1 失败 password=plain123";
        let (clean, changed) = scrub_text(raw, &secrets());
        assert!(changed);
        assert!(clean.contains("shop"));
        assert!(clean.contains("orders"));
        assert!(clean.contains("idx_created"));
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
        for leaked in ["rootpass", "127.0.0.1:3306", "plain123", "mysql://", "password="] {
            assert!(!clean.contains(leaked), "leaked {leaked}: {clean}");
        }
    }

    #[test]
    fn removes_bare_userinfo_and_host_port() {
        let raw = "user:pass@db.internal:3306/app 与 ops@10.0.0.5:5432 均不可用";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(changed);
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
        assert!(!clean.contains("db.internal:3306"));
        assert!(!clean.contains("10.0.0.5:5432"));
    }

    #[test]
    fn keeps_normal_emails_and_names() {
        let raw = "表 orders 的索引 idx_orders_created_at 连续 3 次为 0，联系人 dba@example.com";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(!changed, "should not change: {clean}");
        assert!(clean.contains("orders"));
        assert!(clean.contains("dba@example.com"));
    }

    #[test]
    fn scrubs_json_recursively() {
        let mut value = json!({
            "message": "mysql://root:pass@127.0.0.1:3306/app",
            "evidence": {"note": "password=abc", "table": "orders"},
        });
        assert!(scrub_json(&mut value, &[]));
        let text = serde_json::to_string(&value).unwrap();
        assert!(!contains_sensitive_shape(&text), "still sensitive: {text}");
        assert!(text.contains("orders"));
    }

    #[test]
    fn scrubs_historical_snapshot_before_display() {
        let item = EvidenceItem::new(
            "orders.idx_created",
            Severity::Warn,
            "mysql://root:pass@127.0.0.1:3306/app password=abc".into(),
            json!({"dsn": "postgres://u:p@localhost:5432/db", "table": "orders"}),
        );
        let check = CheckResult {
            check_id: "unused_index".into(),
            title: "未使用索引".into(),
            status: CheckStatus::Warning,
            rule: "r".into(),
            summary: "库 shop 的表 orders".into(),
            evidence: vec![item],
            checked_objects: 1,
            duration_ms: 1,
            skipped_reason: Some("mysql://root:pass@127.0.0.1:3306/app".into()),
        };
        let mut snapshot = InspectionSnapshot {
            snapshot_id: "s1".into(),
            db_id: "db".into(),
            db_name: "shop".into(),
            db_fingerprint: "fp".into(),
            started_at: 1,
            finished_at: 2,
            trigger: "manual".into(),
            status: SnapshotStatus::Warning,
            check_ids: vec!["unused_index".into()],
            checks: vec![check],
        };
        assert!(scrub_snapshot(&mut snapshot, &[]));
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(!contains_sensitive_shape(&text), "still sensitive: {text}");
        assert!(text.contains("shop"));
        assert!(text.contains("orders"));
    }

    #[test]
    fn scrubs_diff_messages() {
        let mut diff = SnapshotDiff {
            previous_snapshot_id: Some("s0".into()),
            current_snapshot_id: "s1".into(),
            added: vec![DiffEvidence {
                check_id: "c".into(),
                evidence_key: "t.i".into(),
                severity: Severity::Warn,
                message: "mysql://root:pass@127.0.0.1:3306/app".into(),
                fields: json!({"password": "abc"}),
            }],
            resolved: Vec::new(),
            changed: Vec::new(),
            check_status_changes: Vec::new(),
        };
        assert!(scrub_diff(&mut diff, &[]));
        let text = serde_json::to_string(&diff).unwrap();
        assert!(!contains_sensitive_shape(&text), "still sensitive: {text}");
    }
}
