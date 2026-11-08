//! V2/V3/V4 差异主键分页（只读）。
//!
//! 生成 SQL 的模板在 `scripts.rs` 中；本模块负责用真实参数执行只读查询，
//! 返回差异总数与当前页主键。所有 SQL 都经过 `fetch_read_only` 的 AST 校验。

use serde::{Deserialize, Serialize};

use crate::ops::error::OpsError;
use crate::ops::mysql_rows::RowMap;
use crate::ops::registry::OpsContext;
use crate::sql::util::{escape_sql_string, quote_ident_mysql, validate_identifier};

use super::read_only::fetch_read_only;
use super::schema::TargetColumn;
use super::scripts::{diff_page_sql, diff_select, diff_total_sql};
use super::types::EmptyStringPolicy;

pub const DEFAULT_PAGE_SIZE: u32 = 100;
pub const MAX_PAGE_SIZE: u32 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPageRequest {
    pub table: String,
    /// 源列（JSON 或标量列）。
    pub column: String,
    pub target_table: String,
    #[serde(default)]
    pub pk_column: Option<String>,
    /// 脚本实际使用的目标列；用于重建比对谓词。
    #[serde(default)]
    pub target_columns: Vec<TargetColumn>,
    #[serde(default)]
    pub empty_string_policy: EmptyStringPolicy,
    /// v2 / v3 / v4
    pub phase: String,
    /// 上一页最后一个主键；首次为空。
    #[serde(default)]
    pub last_pk: Option<String>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPage {
    pub phase: String,
    pub total: u64,
    pub page_size: u32,
    pub pk_column: String,
    pub last_pk: Option<String>,
    pub next_last_pk: Option<String>,
    pub has_more: bool,
    pub pks: Vec<String>,
}

pub async fn fetch_page(
    ctx: &OpsContext,
    request: &DiffPageRequest,
) -> Result<DiffPage, OpsError> {
    let phase = request.phase.trim().to_ascii_lowercase();
    if !matches!(phase.as_str(), "v2" | "v3" | "v4") {
        return Err(OpsError::Validation(format!(
            "未知差异阶段 {}，仅支持 v2/v3/v4",
            request.phase
        )));
    }
    if request.target_columns.is_empty() {
        return Err(OpsError::Validation(
            "缺少目标列快照，无法重建校验谓词".into(),
        ));
    }
    for name in [&request.table, &request.target_table, &request.column] {
        validate_identifier(name)
            .map_err(|err| OpsError::Validation(format!("标识符非法 {name}: {err}")))?;
    }
    let pk_name = request
        .pk_column
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("id");
    validate_identifier(pk_name)
        .map_err(|err| OpsError::Validation(format!("主键列非法 {pk_name}: {err}")))?;
    for column in &request.target_columns {
        validate_identifier(&column.name)
            .map_err(|err| OpsError::Validation(format!("目标列非法 {}: {err}", column.name)))?;
    }
    let page_size = request
        .page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);

    let source = format!(
        "{}.{}",
        quote_ident_mysql(&ctx.db_name),
        quote_ident_mysql(request.table.trim())
    );
    let destination = format!(
        "{}.{}",
        quote_ident_mysql(&ctx.db_name),
        quote_ident_mysql(request.target_table.trim())
    );
    let source_column = quote_ident_mysql(request.column.trim());
    let pk = quote_ident_mysql(pk_name);

    let select = diff_select(
        &phase,
        &source,
        &destination,
        &source_column,
        &pk,
        &request.target_columns,
        request.empty_string_policy,
    );
    let total_sql = diff_total_sql(&select);
    let total_rows = fetch_read_only(ctx, &total_sql).await?;
    let total = total_rows
        .first()
        .and_then(|row| row.get("diff_total"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let last_pk = request
        .last_pk
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let last_literal = last_pk.map(|value| format!("'{}'", escape_sql_string(value)));
    let key_expr = if phase == "v3" {
        format!("t.{pk}")
    } else {
        format!("s.{pk}")
    };
    let page_sql = diff_page_sql(
        &select,
        &key_expr,
        last_literal.as_deref(),
        &(page_size + 1).to_string(),
    );
    let rows = fetch_read_only(ctx, &page_sql).await?;
    let mut pks = page_pks(&rows);
    let has_more = pks.len() as u32 > page_size;
    if has_more {
        pks.truncate(page_size as usize);
    }
    let next_last_pk = pks.last().cloned();

    Ok(DiffPage {
        phase,
        total,
        page_size,
        pk_column: pk_name.to_string(),
        last_pk: last_pk.map(str::to_string),
        next_last_pk,
        has_more,
        pks,
    })
}

fn page_pks(rows: &[RowMap]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::{scripts, types::EmptyStringPolicy};

    fn columns() -> Vec<TargetColumn> {
        vec![TargetColumn {
            name: "raw_text".into(),
            source_key: None,
            sql_type: "VARCHAR(1024)".into(),
            nullable: true,
            required: false,
            note: None,
        }]
    }

    #[test]
    fn page_sql_uses_primary_key_interval_instead_of_fixed_limit() {
        let select = scripts::diff_select(
            "v2",
            "`app`.`orders`",
            "`app`.`orders_ext`",
            "`payload`",
            "`id`",
            &columns(),
            EmptyStringPolicy::Preserve,
        );
        let page = scripts::diff_page_sql(&select, "s.`id`", Some("'12345'"), "100");
        assert!(page.contains("s.`id` > '12345'"));
        assert!(page.contains("ORDER BY s.`id`"));
        assert!(page.contains("LIMIT 100"));
    }

    #[test]
    fn total_sql_wraps_diff_without_limit() {
        let select = scripts::diff_select(
            "v3",
            "`app`.`orders`",
            "`app`.`orders_ext`",
            "`payload`",
            "`id`",
            &columns(),
            EmptyStringPolicy::Preserve,
        );
        let total = scripts::diff_total_sql(&select);
        assert!(total.contains("COUNT(*) AS diff_total"));
        assert!(!total.to_ascii_uppercase().contains("LIMIT"));
    }

    #[test]
    fn scalar_verify_distinguishes_empty_string_from_null() {
        let select = scripts::diff_select(
            "v4",
            "`app`.`orders`",
            "`app`.`orders_ext`",
            "`raw_text`",
            "`id`",
            &columns(),
            EmptyStringPolicy::Preserve,
        );
        assert!(select.contains("s.`raw_text` IS NULL AND t.`raw_text` IS NULL"));
        assert!(select.contains("CAST(s.`raw_text` AS BINARY)"));
        assert!(!select.contains("NULLIF(s.`raw_text`"));
    }
}
