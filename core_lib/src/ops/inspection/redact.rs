//! 巡检敏感信息清洗。
//!
//! 两条防线：
//! 1. 源头：巡检快照落库前清洗 message / evidence / skipped_reason，避免凭据进入历史记录；
//! 2. 展示与通知：历史数据返回界面、写审计、发邮件/Webhook 前再次强制清洗。
//!
//! 清洗是“删除”而不是“替换成 ***”，避免保留 `user:pass@` 这类可辨认的凭据结构；
//! 但主机名、端口、库名、账号名与驱动错误码/原因属于排查必需的非敏感信息，
//! 必须原样保留（例如 `1045 Access denied for user 'shop'@'localhost'`）。

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

/// 凭据键名，按长度从长到短排列，保证 `secret_access_key` 先于 `secret` 匹配。
const SECRET_KEYS: &[&str] = &[
    "secret_access_key",
    "access_key_id",
    "access_key",
    "secret_key",
    "api_key",
    "authorization",
    "password",
    "passwd",
    "pwd",
    "apikey",
    "accesskey",
    "secretkey",
    "token",
    "secret",
    "ak",
    "sk",
];

/// 密码键：值若是 YES/NO 这类“是否提供密码”的诊断信息，必须保留。
const PASSWORD_KEYS: [&str; 3] = ["password", "passwd", "pwd"];

fn is_password_key(key: &str) -> bool {
    PASSWORD_KEYS.contains(&key)
}

/// `using password: YES` 这类诊断值不是凭据本体。
fn is_password_presence_diagnostic(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "YES" | "NO" | "TRUE" | "FALSE"
    )
}

/// 删除标记：删除敏感片段后先留下标记，最后统一清理悬空标签与标点。
const CUT: char = '\u{1f}';

/// 常见“标签”词：删除其后的连接串/凭据后，这些词会变成断头句。
const DANGLING_LABELS: [&str; 33] = [
    "connection string",
    "authorization",
    "connection",
    "hostname",
    "username",
    "password",
    "passwd",
    "secret",
    "server",
    "endpoint",
    "address",
    "token",
    "bearer",
    "host",
    "user",
    "basic",
    "pwd",
    "url",
    "uri",
    "dsn",
    "addr",
    "数据库地址",
    "连接串",
    "连接地址",
    "服务地址",
    "连接",
    "地址",
    "主机",
    "参数",
    "密码",
    "口令",
    "用户名",
    "账号",
];

fn push_with_cut(output: &mut String, head: &[char], tail: &[char]) {
    output.extend(head);
    output.push(CUT);
    output.extend(tail);
}

/// 只删除 `user:pass@` 里的 `:pass`，保留账号、主机、端口与库名。
///
/// 覆盖 `scheme://user:pass@host:port/db` 与裸 `user:pass@host` 两种形态；
/// 邮箱（`dba@example.com`）与驱动错误里的 `'shop'@'localhost'` 不受影响。
fn strip_userinfo_passwords(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '@' {
            index += 1;
            continue;
        }
        let mut start = index;
        while start > 0 && !is_delimiter(chars[start - 1]) {
            start -= 1;
        }
        // URI 的 scheme 里也有冒号，真正的 userinfo 从最后一个 `://` 之后开始。
        let mut user_start = start;
        let mut probe = start;
        while probe + 2 < index {
            if chars[probe] == ':' && chars[probe + 1] == '/' && chars[probe + 2] == '/' {
                user_start = probe + 3;
            }
            probe += 1;
        }
        if let Some(offset) = chars[user_start..index].iter().position(|c| *c == ':') {
            let colon = user_start + offset;
            let mut next = String::new();
            next.extend(&chars[..colon]);
            next.extend(&chars[index..]);
            return strip_userinfo_passwords(&next);
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
        let boundary =
            index == 0 || is_delimiter(chars[index - 1]) || matches!(chars[index - 1], '&' | '?');
        if boundary {
            let rest: String = chars[index..]
                .iter()
                .take(24)
                .collect::<String>()
                .to_ascii_lowercase();
            let mut matched_key = None;
            for &key in SECRET_KEYS {
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
                        let value: String = chars[cursor + 1..end]
                            .iter()
                            .collect::<String>()
                            .trim()
                            .to_string();
                        if value.is_empty()
                            || (is_password_key(key) && is_password_presence_diagnostic(&value))
                        {
                            index += 1;
                            continue;
                        }
                        // URL 查询参数：把 `?password=x&next=1` 清理成 `?next=1`，
                        // 不留下悬空的 `?`/`&`。
                        if index > 0 && matches!(chars[index - 1], '?' | '&') {
                            let sep = chars[index - 1];
                            let mut next = String::new();
                            next.extend(&chars[..index - 1]);
                            if end < chars.len() && chars[end] == '&' {
                                next.push(sep);
                                next.extend(&chars[end + 1..]);
                            } else {
                                next.extend(&chars[end..]);
                            }
                            return remove_secret_assignments(&next);
                        }
                        let mut next = String::new();
                        push_with_cut(&mut next, &chars[..index], &chars[end..]);
                        return remove_secret_assignments(&next);
                    }
                }
            }
        }
        index += 1;
    }
    input.to_string()
}

fn separator(value: char) -> bool {
    matches!(value, ';' | ',' | ':' | '；' | '，' | '、')
}

fn opening_bracket(value: char) -> bool {
    matches!(value, '(' | '[' | '{' | '（' | '【' | '《')
}

fn closing_bracket(value: char) -> bool {
    matches!(value, ')' | ']' | '}' | '）' | '】' | '》')
}

fn trim_end_with<F: Fn(char) -> bool>(chars: &[char], predicate: F) -> usize {
    let mut end = chars.len();
    while end > 0 && predicate(chars[end - 1]) {
        end -= 1;
    }
    end
}

/// 找到紧邻删除标记之前的标签词，返回应删除的起始位置。
fn dangling_label_start(chars: &[char], cut: usize) -> Option<usize> {
    let mut end = cut;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end > 0 && matches!(chars[end - 1], ':' | '=' | '：' | '＝') {
        end -= 1;
        while end > 0 && chars[end - 1].is_whitespace() {
            end -= 1;
        }
    }
    for label in DANGLING_LABELS {
        let label_chars: Vec<char> = label.chars().collect();
        if label_chars.len() > end {
            continue;
        }
        let start = end - label_chars.len();
        let matches = chars[start..end]
            .iter()
            .zip(&label_chars)
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase());
        if !matches {
            continue;
        }
        let boundary = start == 0
            || is_delimiter(chars[start - 1])
            || separator(chars[start - 1])
            || opening_bracket(chars[start - 1]);
        if boundary {
            return Some(start);
        }
    }
    None
}

/// 删除切点后残留的悬空标签、空括号与多余标点。
fn tidy_cut_markers(input: &str) -> String {
    let mut chars: Vec<char> = input.chars().collect();

    // 1. 删除紧挨切点的标签词（DSN、参数：、password= 等）。
    loop {
        let position = chars.iter().position(|c| *c == CUT);
        let Some(cut) = position else { break };
        let start = dangling_label_start(&chars, cut).unwrap_or(cut);
        chars.drain(start..=cut);
    }

    // 2. 切点变成空格，避免把前后单词粘在一起。
    for value in &mut chars {
        if *value == CUT {
            *value = ' ';
        }
    }

    // 3. 去掉空括号与括号内只剩标点的情况。
    loop {
        let mut changed = false;
        let mut index = 0;
        while index < chars.len() {
            if opening_bracket(chars[index]) {
                let mut end = index + 1;
                while end < chars.len() && chars[end].is_whitespace() {
                    end += 1;
                }
                let mut close = end;
                while close < chars.len() && separator(chars[close]) {
                    close += 1;
                }
                if close < chars.len() && closing_bracket(chars[close]) {
                    chars.drain(index..=close);
                    changed = true;
                    continue;
                }
                if end < chars.len() && closing_bracket(chars[end]) {
                    chars.drain(index..=end);
                    changed = true;
                    continue;
                }
            }
            index += 1;
        }
        if !changed {
            break;
        }
    }

    // 4. 标点贴括号/句号时视为空段，去掉标点。
    let mut index = 0;
    while index < chars.len() {
        if separator(chars[index]) {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            let before_open = index > 0
                && (opening_bracket(chars[index - 1])
                    || chars[..index]
                        .iter()
                        .rev()
                        .find(|c| !c.is_whitespace())
                        .map(|c| opening_bracket(*c))
                        .unwrap_or(false));
            let before_close = next < chars.len() && closing_bracket(chars[next]);
            let before_end = next >= chars.len();
            let after_sentence = next < chars.len() && matches!(chars[next], '。' | '.');
            if before_open || before_close || before_end || after_sentence {
                chars.remove(index);
                continue;
            }
        }
        index += 1;
    }

    // 5. 连续标点只保留一个，但不改动正常的“逗号+空格”。
    let mut index = 0;
    while index < chars.len() {
        if separator(chars[index]) {
            let mut probe = index + 1;
            while probe < chars.len() && chars[probe].is_whitespace() {
                probe += 1;
            }
            if probe < chars.len() && separator(chars[probe]) {
                let mut end = probe + 1;
                while end < chars.len() && (chars[end].is_whitespace() || separator(chars[end])) {
                    end += 1;
                }
                chars.drain(index + 1..end);
            }
        }
        index += 1;
    }

    // 6. 合并空格（保留换行）。
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ' ' || chars[index] == '\t' {
            let mut end = index + 1;
            while end < chars.len() && (chars[end] == ' ' || chars[end] == '\t') {
                end += 1;
            }
            if end > index + 1 {
                chars.drain(index + 1..end);
            }
        }
        index += 1;
    }

    let start = chars
        .iter()
        .position(|c| !c.is_whitespace() && !separator(*c))
        .unwrap_or(chars.len());
    let end = trim_end_with(&chars, |c| c.is_whitespace() || separator(c));
    if start >= end {
        return String::new();
    }
    chars[start..end].iter().collect::<String>()
}

/// 强制清洗文本。返回 `(清洗后的文本, 是否发生变化)`。
///
/// 只删除凭据本体，保留驱动错误码、原因、账号、主机、端口与库名。
pub fn scrub_text(input: &str, secrets: &[String]) -> (String, bool) {
    let mut output = input.to_string();
    let cut = CUT.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            output = output.replace(secret.as_str(), &cut);
        }
    }
    output = strip_userinfo_passwords(&output);
    output = remove_secret_assignments(&output);
    output = tidy_cut_markers(&output);
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

/// 是否仍包含凭据本体；用于测试与运行期断言。
///
/// 主机名、端口、库名、账号名与 `using password: YES` 这类诊断信息不算泄漏，
/// 因此不再把裸 URI 或 `@host:port` 视为敏感形态。
pub fn contains_sensitive_shape(text: &str) -> bool {
    has_userinfo_password(text) || has_secret_assignment(text)
}

/// `scheme://user:pass@host` 或裸 `user:pass@host` 里是否仍带密码。
fn has_userinfo_password(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for index in 0..chars.len() {
        if chars[index] != '@' {
            continue;
        }
        let mut start = index;
        while start > 0 && !is_delimiter(chars[start - 1]) {
            start -= 1;
        }
        let mut user_start = start;
        let mut probe = start;
        while probe + 2 < index {
            if chars[probe] == ':' && chars[probe + 1] == '/' && chars[probe + 2] == '/' {
                user_start = probe + 3;
            }
            probe += 1;
        }
        if chars[user_start..index].contains(&':') {
            return true;
        }
    }
    false
}

/// 是否仍存在带非空值（且不是诊断值）的凭据键值。
fn has_secret_assignment(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let boundary =
            index == 0 || is_delimiter(chars[index - 1]) || matches!(chars[index - 1], '&' | '?');
        if boundary {
            let rest: String = chars[index..]
                .iter()
                .take(24)
                .collect::<String>()
                .to_ascii_lowercase();
            for &key in SECRET_KEYS {
                if !rest.starts_with(key) {
                    continue;
                }
                let after_key = index + key.len();
                let after_boundary = after_key == chars.len()
                    || is_delimiter(chars[after_key])
                    || matches!(chars[after_key], '=' | ':');
                if !after_boundary {
                    continue;
                }
                let mut cursor = after_key;
                while cursor < chars.len() && chars[cursor] == ' ' {
                    cursor += 1;
                }
                if cursor >= chars.len() || !matches!(chars[cursor], '=' | ':') {
                    continue;
                }
                let mut end = cursor + 1;
                while end < chars.len() && chars[end] == ' ' {
                    end += 1;
                }
                if key == "authorization" {
                    while end < chars.len() && chars[end] != '\n' {
                        end += 1;
                    }
                } else {
                    while end < chars.len() && !is_delimiter(chars[end]) && chars[end] != '&' {
                        end += 1;
                    }
                }
                let value: String = chars[cursor + 1..end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if value.is_empty()
                    || (is_password_key(key) && is_password_presence_diagnostic(&value))
                {
                    continue;
                }
                return true;
            }
        }
        index += 1;
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
        // 主机、端口、库名与账号是排查必需信息，必须保留；只删除密码本体。
        assert!(
            clean.contains("mysql://root@127.0.0.1:3306/shop?ssl=1"),
            "connection skeleton should be kept: {clean}"
        );
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
        for leaked in ["rootpass", "plain123", "password="] {
            assert!(!clean.contains(leaked), "leaked {leaked}: {clean}");
        }
    }

    #[test]
    fn removes_userinfo_password_but_keeps_host_port() {
        let raw = "user:pass@db.internal:3306/app 与 ops@10.0.0.5:5432 均不可用";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(changed);
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
        assert!(!clean.contains("pass@"), "password body leaked: {clean}");
        assert!(clean.contains("user@db.internal:3306/app"), "{clean}");
        assert!(clean.contains("ops@10.0.0.5:5432"), "{clean}");
    }

    #[test]
    fn keeps_driver_error_code_and_reason() {
        let raw = "Failed to connect to database: 1045 Access denied for user 'shop'@'localhost' (using password: YES)";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(!changed, "auth failure diagnostics must be kept: {clean}");
        for expected in ["1045", "Access denied", "shop", "localhost", "using password: YES"] {
            assert!(clean.contains(expected), "missing {expected}: {clean}");
        }
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
    }

    #[test]
    fn removes_secret_assignments_and_keeps_uri_skeleton() {
        let raw = "连接 mysql://root@127.0.0.1:3306/shop?password=plain123&ssl=1 失败；token:abc456 secret_access_key=AKIAEXAMPLE";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(changed);
        assert!(
            clean.contains("mysql://root@127.0.0.1:3306/shop?ssl=1"),
            "{clean}"
        );
        for leaked in ["plain123", "password=", "abc456", "token:", "AKIAEXAMPLE"] {
            assert!(!clean.contains(leaked), "leaked {leaked}: {clean}");
        }
        assert!(!clean.contains("?&"), "dangling query separator: {clean}");
        assert!(!contains_sensitive_shape(&clean), "still sensitive: {clean}");
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
            not_applicable: Vec::new(),
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

    #[test]
    fn keeps_context_label_and_separator_without_credentials() {
        let raw = "索引 idx_created 与 idx_created_at 重复；DSN mysql://root:pass@127.0.0.1:3306/shop?password=plain123";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(changed);
        assert_eq!(
            clean,
            "索引 idx_created 与 idx_created_at 重复；DSN mysql://root@127.0.0.1:3306/shop"
        );
        assert!(!clean.contains("pass@"), "{clean}");
        assert!(!clean.contains("plain123"), "{clean}");
        assert!(!clean.ends_with('；'));
    }

    #[test]
    fn keeps_parameter_label_and_connection_skeleton() {
        let raw = "采集失败（参数：mysql://root:pass@127.0.0.1:3306/shop）";
        let (clean, _) = scrub_text(raw, &[]);
        assert_eq!(clean, "采集失败（参数：mysql://root@127.0.0.1:3306/shop）");
        assert!(!clean.contains("pass@"), "{clean}");
        assert!(!clean.contains("（）"));
    }

    #[test]
    fn keeps_host_port_and_clock_time() {
        let (clean, changed) = scrub_text("连接 10.0.0.5:5432 失败", &[]);
        assert!(!changed, "host:port is diagnostic info: {clean}");
        assert!(clean.contains("10.0.0.5:5432"), "{clean}");

        let (clock, changed) = scrub_text("告警时间 12:30 触发", &[]);
        assert!(!changed, "clock should be untouched: {clock}");
        assert!(clock.contains("12:30"));
    }

    #[test]
    fn keeps_normal_punctuation_and_spacing() {
        let raw = "库 shop 的表 orders，索引 idx_created 连续 3 次为 0。";
        let (clean, changed) = scrub_text(raw, &[]);
        assert!(!changed, "should not change: {clean}");
        assert_eq!(clean, raw);
    }

    #[derive(serde::Deserialize)]
    struct RedactVector {
        name: String,
        input: String,
        #[serde(default)]
        secrets: Vec<String>,
        must_disappear: Vec<String>,
        must_keep: Vec<String>,
        #[serde(default)]
        unchanged: bool,
    }

    #[derive(serde::Deserialize)]
    struct RedactFixture {
        vectors: Vec<RedactVector>,
    }

    /// 与 web-ui/src/utils/redact.ts 共用同一份向量，防止两侧规则漂移。
    #[test]
    fn shared_fixture_vectors_match_ts_implementation() {
        let raw = include_str!("../../../../web-ui/src/utils/__fixtures__/redact-vectors.json");
        let fixture: RedactFixture = serde_json::from_str(raw).expect("vector fixture should parse");
        assert!(!fixture.vectors.is_empty(), "fixture should not be empty");
        for vector in fixture.vectors {
            let (clean, changed) = scrub_text(&vector.input, &vector.secrets);
            if vector.unchanged {
                assert!(!changed, "{}: should be unchanged: {clean}", vector.name);
            }
            for leaked in &vector.must_disappear {
                assert!(
                    !clean.contains(leaked),
                    "{}: leaked {leaked}: {clean}",
                    vector.name
                );
            }
            for kept in &vector.must_keep {
                assert!(
                    clean.contains(kept),
                    "{}: missing {kept}: {clean}",
                    vector.name
                );
            }
            assert!(
                !clean.contains("***"),
                "{}: should not emit masks: {clean}",
                vector.name
            );
            assert!(
                !contains_sensitive_shape(&clean),
                "{}: still sensitive: {clean}",
                vector.name
            );
        }
    }
}
