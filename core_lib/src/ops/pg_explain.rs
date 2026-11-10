//! PostgreSQL `EXPLAIN (FORMAT JSON)` 解析。
//!
//! 巡检/性能诊断都在用同一份映射，避免同一个节点在“索引优化”和“性能诊断”
//! 里被解释成不同含义。

use serde_json::Value;

use crate::ops::types::ExplainSample;

/// 取 `EXPLAIN (FORMAT JSON)` 的根 Plan 节点。
pub(crate) fn parse_root(raw: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|err| format!("EXPLAIN JSON 解析失败: {err}"))?;
    let array = value
        .as_array()
        .ok_or_else(|| "EXPLAIN JSON 顶层不是数组".to_string())?;
    let first = array.first().ok_or_else(|| "EXPLAIN JSON 为空".to_string())?;
    first
        .get("Plan")
        .cloned()
        .ok_or_else(|| "EXPLAIN JSON 缺少 Plan 节点".to_string())
}

/// 深度优先展开所有 Plan 节点（含根）。
pub(crate) fn walk_nodes(root: &Value) -> Vec<Value> {
    let mut nodes = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        if let Some(children) = node.get("Plans").and_then(|value| value.as_array()) {
            for child in children.iter().rev() {
                stack.push(child.clone());
            }
        }
        nodes.push(node);
    }
    nodes
}

pub(crate) fn sample_from_node(label: &str, sql: &str, node: &Value) -> ExplainSample {
    let node_type = node
        .get("Node Type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let access_type = if node_type.eq_ignore_ascii_case("Seq Scan") {
        "ALL".to_string()
    } else {
        node_type.to_string()
    };
    let key = node
        .get("Index Name")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let rows_examined = node
        .get("Plan Rows")
        .and_then(|value| value.as_i64())
        .or_else(|| {
            node.get("Plan Rows")
                .and_then(|value| value.as_f64())
                .map(|value| value as i64)
        });
    let actual_rows = node.get("Actual Rows").and_then(|value| value.as_f64());
    let filtered = match (rows_examined, actual_rows) {
        (Some(planned), Some(actual)) if planned > 0 => {
            Some((actual / planned as f64 * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let mut extra_parts = Vec::new();
    for key_name in [
        "Filter",
        "Index Cond",
        "Hash Cond",
        "Merge Cond",
        "Join Type",
        "Sort Key",
        "Group Key",
        "Recheck Cond",
    ] {
        if let Some(value) = node.get(key_name) {
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            if !text.is_empty() {
                extra_parts.push(format!("{key_name}={text}"));
            }
        }
    }
    if node
        .get("Sort Method")
        .and_then(|value| value.as_str())
        .map(|value| value.to_ascii_lowercase().contains("external"))
        .unwrap_or(false)
    {
        extra_parts.push("using temporary (external sort)".into());
    }
    ExplainSample {
        label: label.to_string(),
        sql: sql.to_string(),
        access_type: Some(access_type),
        key,
        possible_keys: None,
        rows_examined,
        filtered,
        extra: (!extra_parts.is_empty()).then(|| extra_parts.join("; ")),
        table_name: node
            .get("Relation Name")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string()),
        estimated_cost: node
            .get("Total Cost")
            .and_then(|value| value.as_f64()),
        actual_time_ms: node
            .get("Actual Total Time")
            .and_then(|value| value.as_f64())
            .or_else(|| node.get("Execution Time").and_then(|value| value.as_f64())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"orders","Plan Rows":1200,"Total Cost":35.5,"Filter":"(status = 'open')"}}]"#;

    #[test]
    fn seq_scan_maps_to_all_and_keeps_cost() {
        let root = parse_root(PLAIN).unwrap();
        let sample = sample_from_node("workload-1", "select * from orders", &root);
        assert_eq!(sample.access_type.as_deref(), Some("ALL"));
        assert_eq!(sample.table_name.as_deref(), Some("orders"));
        assert_eq!(sample.rows_examined, Some(1200));
        assert_eq!(sample.estimated_cost, Some(35.5));
        assert!(sample.extra.as_ref().unwrap().contains("Filter"));
        assert!(sample.needs_index());
    }

    #[test]
    fn index_scan_does_not_need_index() {
        let raw = r#"[{"Plan":{"Node Type":"Index Scan","Index Name":"idx_orders_status","Relation Name":"orders","Plan Rows":10,"Total Cost":8.2}}]"#;
        let root = parse_root(raw).unwrap();
        let sample = sample_from_node("workload-1", "select 1", &root);
        assert_eq!(sample.key.as_deref(), Some("idx_orders_status"));
        assert!(!sample.needs_index());
    }

    #[test]
    fn walk_nodes_includes_children() {
        let raw = r#"[{"Plan":{"Node Type":"Hash Join","Plans":[{"Node Type":"Seq Scan","Relation Name":"a"},{"Node Type":"Seq Scan","Relation Name":"b"}]}}]"#;
        let root = parse_root(raw).unwrap();
        let nodes = walk_nodes(&root);
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_root("not json").is_err());
        assert!(parse_root("[]").is_err());
    }
}
