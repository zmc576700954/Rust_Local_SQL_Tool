//! SchemaService — Schema 查询 / 缓存 / 虚拟 Schema 解析
//!
//! 从 web-server/src/main.rs 中提取 schema 相关业务逻辑，
//! 使 web-server 和 src-tauri 可共用。

use std::time::{Duration, Instant};

use crate::db::DbClient;
use crate::schema::{SchemaExtractor, SchemaResponse, TableWithDetails};
use crate::sql::offline_parser::OfflineParser;
use crate::sql::util::quote_ident_mysql_checked;
use crate::service::sort_expr::sanitize_sort_expression;
use crate::service::context::{
    CachedDbClient, CachedSchemaEntry, CachedTableSchemaEntry, ServiceContext,
};
use crate::service::error::ServiceError;
use crate::service::row_codec::{encode_mysql_row, MySqlRowJsonEncoder, bind_json_value_to_query};

// ── 常量 ────────────────────────────────────────────────

const SCHEMA_CACHE_TTL: Duration = Duration::from_secs(30);
const TABLE_SCHEMA_CACHE_TTL: Duration = Duration::from_secs(300);
const DB_CLIENT_CACHE_TTL: Duration = Duration::from_secs(600);

// ── 参数 / 响应类型 ─────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GetTableDataParams {
    pub table_name: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub filters: Option<String>,
    pub orders: Option<String>,
    pub db_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GetTableDataResult {
    pub data: Vec<serde_json::Value>,
    pub total: Option<i64>,
    pub total_status: String,
    pub has_more: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FilterCondition {
    pub column: String,
    pub operator: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderKind {
    #[default]
    Column,
    Expression,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderNulls {
    #[default]
    Default,
    First,
    Last,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OrderCondition {
    #[serde(default)]
    pub kind: OrderKind,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub expression: Option<String>,
    pub desc: bool,
    #[serde(default)]
    pub nulls: OrderNulls,
}

// ── SchemaService ───────────────────────────────────────

pub struct SchemaService;

impl SchemaService {
    // ── Schema 查询 ──────────────────────────────────────

    /// 获取 schema（活跃 DB 或指定 db_id）
    pub async fn get_schema(
        ctx: &ServiceContext,
        db_id: Option<&str>,
    ) -> Result<SchemaResponse, ServiceError> {
        if let Some(id) = db_id {
            return Self::get_schema_for_db_id(ctx, id).await;
        }
        Self::get_schema_internal(ctx).await
    }

    /// 解析虚拟 Schema（DDL → SchemaResponse）并存入 AiState
    pub async fn parse_virtual_schema(
        ctx: &ServiceContext,
        sql_content: &str,
    ) -> Result<SchemaResponse, ServiceError> {
        let schema = OfflineParser::parse_sql(sql_content).map_err(ServiceError::ParseError)?;
        ctx.ai_state().set_virtual_schema(schema.clone()).await;
        Ok(schema)
    }

    /// 获取指定表的结构（含缓存）
    pub async fn get_table_schema(
        ctx: &ServiceContext,
        db_id: Option<&str>,
        table_name: &str,
    ) -> Result<TableWithDetails, ServiceError> {
        let (db_client, db_name) = Self::resolve_db_client(ctx, db_id).await?;
        Self::get_cached_table_schema(ctx, db_id, &db_client, &db_name, table_name).await
    }

    /// 获取表数据（动态 SELECT + WHERE / ORDER BY / LIMIT / OFFSET）
    pub async fn get_table_data(
        ctx: &ServiceContext,
        params: GetTableDataParams,
    ) -> Result<GetTableDataResult, ServiceError> {
        let (db_client, _) = Self::resolve_db_client(ctx, params.db_id.as_deref()).await?;
        let page = params.page.unwrap_or(1);
        let page_size = params.page_size.unwrap_or(100);
        let offset = (page - 1) * page_size;

        let (where_clause, bindings) = Self::build_where_clause(&params.filters)?;
        let order_clause = Self::build_order_clause(&params.orders)?;

        let table_ident = quote_ident_mysql_checked(&params.table_name)
            .map_err(ServiceError::BadRequest)?;
        let data_sql = format!(
            "SELECT * FROM {} {} {} LIMIT {} OFFSET {}",
            table_ident, where_clause, order_clause, page_size + 1, offset
        );

        let mut data_query = sqlx::query(&data_sql);
        for b in &bindings {
            data_query = bind_json_value_to_query(data_query, &serde_json::Value::String(b.clone()));
        }

        let result_rows = tokio::time::timeout(
            ctx.timeouts().db_query,
            data_query.fetch_all(db_client.mysql_pool()?),
        )
        .await
        .map_err(|_| ServiceError::Timeout("Query timed out".into()))?
        .map_err(ServiceError::from)?;

        let has_more = result_rows.len() as u32 > page_size;
        let mut rows = Vec::new();
        let mut row_encoder = None;
        for row in result_rows.into_iter().take(page_size as usize) {
            if row_encoder.is_none() {
                row_encoder = Some(MySqlRowJsonEncoder::from_row(&row));
            }
            rows.push(encode_mysql_row(
                &row,
                row_encoder.as_ref().expect("row encoder initialized"),
            ));
        }

        Ok(GetTableDataResult {
            data: rows,
            total: None,
            total_status: "calculating".to_string(),
            has_more,
        })
    }

    /// 清除所有 metadata 缓存
    pub async fn clear_metadata_caches(ctx: &ServiceContext) {
        ctx.schema_state().clear_all().await;
    }

    // ── 内部方法 ──────────────────────────────────────────

    /// 获取活跃 DB 的 schema（虚拟 schema 优先）
    async fn get_schema_internal(ctx: &ServiceContext) -> Result<SchemaResponse, ServiceError> {
        if let Some(vs) = ctx.ai_state().get_virtual_schema().await {
            return Ok(vs);
        }
        let db_client = ctx
            .db_state()
            .get_active_client()
            .await
            .ok_or_else(|| ServiceError::BadRequest("Database not connected".into()))?;
        let url = ctx.get_config().await.get_active_db_url().unwrap_or_default();
        let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
        Self::get_cached_schema(ctx, None, &db_client, &db_name)
            .await
            .ok_or_else(|| ServiceError::InternalError("Failed to fetch schema".into()))
    }

    /// 获取指定 db_id 的 schema
    async fn get_schema_for_db_id(
        ctx: &ServiceContext,
        db_id: &str,
    ) -> Result<SchemaResponse, ServiceError> {
        let (db_client, db_name) = Self::resolve_db_client(ctx, Some(db_id)).await?;
        Self::get_cached_schema(ctx, Some(db_id), &db_client, &db_name)
            .await
            .ok_or_else(|| ServiceError::InternalError("Failed to fetch schema".into()))
    }

    /// 从 DB 获取 schema（带 TTL 缓存）
    async fn get_cached_schema(
        ctx: &ServiceContext,
        db_id: Option<&str>,
        db_client: &DbClient,
        db_name: &str,
    ) -> Option<SchemaResponse> {
        let key = Self::schema_cache_key(db_id, db_name);
        // 快路径：读锁
        {
            if let Some(entry) = ctx.schema_state().get_schema(&key).await {
                if entry.expires_at > Instant::now() {
                    return Some(entry.schema.clone());
                }
            }
        }
        // 慢路径：写锁 + double-check
        let schema = Self::fetch_schema_for_db(db_client, db_name).await?;
        ctx.schema_state().insert_schema(
            key,
            CachedSchemaEntry {
                schema: schema.clone(),
                expires_at: Instant::now() + SCHEMA_CACHE_TTL,
            },
        )
        .await;
        Some(schema)
    }

    /// 从 DB 获取指定表结构（带 TTL 缓存）
    async fn get_cached_table_schema(
        ctx: &ServiceContext,
        db_id: Option<&str>,
        db_client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<TableWithDetails, ServiceError> {
        let key = Self::table_schema_cache_key(db_id, db_name, table_name);
        // 快路径
        {
            if let Some(entry) = ctx.schema_state().get_table_schema(&key).await {
                if entry.expires_at > Instant::now() {
                    return Ok(entry.table.clone());
                }
            }
        }
        // 慢路径
        let columns = SchemaExtractor::get_columns(db_client, db_name, table_name)
            .await
            .map_err(|e| ServiceError::DbQuery(e.to_string()))?;
        let indexes = SchemaExtractor::get_indexes(db_client, db_name, table_name)
            .await
            .unwrap_or_default();
        let foreign_keys = SchemaExtractor::get_foreign_keys(db_client, db_name, table_name)
            .await
            .unwrap_or_default();
        let table = TableWithDetails {
            table_name: table_name.to_string(),
            columns,
            indexes,
            foreign_keys,
        };
        ctx.schema_state().insert_table_schema(
            key,
            CachedTableSchemaEntry {
                table: table.clone(),
                expires_at: Instant::now() + TABLE_SCHEMA_CACHE_TTL,
            },
        )
        .await;
        Ok(table)
    }

    /// 直接从 DB 拉取全库 schema
    async fn fetch_schema_for_db(
        db_client: &DbClient,
        db_name: &str,
    ) -> Option<SchemaResponse> {
        let tables = SchemaExtractor::get_tables(db_client, db_name).await.ok()?;
        let columns_map = SchemaExtractor::get_columns_map(db_client, db_name)
            .await
            .unwrap_or_default();
        let indexes_map = SchemaExtractor::get_indexes_map(db_client, db_name)
            .await
            .unwrap_or_default();
        let foreign_keys_map = SchemaExtractor::get_foreign_keys_map(db_client, db_name)
            .await
            .unwrap_or_default();

        let mut result_tables = Vec::with_capacity(tables.len());
        for t in tables {
            let table_name = t.table_name;
            let columns = columns_map.get(&table_name).cloned().unwrap_or_default();
            let indexes = indexes_map.get(&table_name).cloned().unwrap_or_default();
            let foreign_keys = foreign_keys_map
                .get(&table_name)
                .cloned()
                .unwrap_or_default();
            result_tables.push(TableWithDetails {
                table_name,
                columns,
                indexes,
                foreign_keys,
            });
        }

        let views = SchemaExtractor::get_views(db_client, db_name)
            .await
            .unwrap_or_default();

        Some(SchemaResponse {
            db_name: db_name.to_string(),
            tables: result_tables,
            views,
        })
    }

    /// 解析 DB 客户端（活跃 / 缓存 / 新建）
    pub(crate) async fn resolve_db_client(
        ctx: &ServiceContext,
        db_id: Option<&str>,
    ) -> Result<(DbClient, String), ServiceError> {
        if let Some(id) = db_id {
            return Self::get_temp_db_client(ctx, id).await;
        }
        let db_client = ctx
            .db_state()
            .get_active_client()
            .await
            .ok_or_else(|| ServiceError::BadRequest("Database not connected".into()))?;
        let url = ctx.get_config().await.get_active_db_url().unwrap_or_default();
        let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
        Ok((db_client, db_name))
    }

    /// 为指定 db_id 获取 DB 客户端（含缓存 + TTL）
    async fn get_temp_db_client(
        ctx: &ServiceContext,
        db_id: &str,
    ) -> Result<(DbClient, String), ServiceError> {
        let config = ctx.get_config().await;
        let conn = config
            .db_connections
            .iter()
            .find(|c| c.id == db_id)
            .ok_or_else(|| ServiceError::BadRequest(format!("Database connection {} not found", db_id)))?;
        let db_name = DbClient::extract_db_name(&conn.url).unwrap_or_default();

        // 如果是活跃连接，直接复用
        if config.active_db_id.as_deref() == Some(db_id) {
            if let Some(client) = ctx.db_state().get_active_client().await {
                return Ok((client, db_name));
            }
        }

        // 检查缓存
        let now = Instant::now();
        if let Some(entry) = ctx.db_state().get_cached_client(db_id).await {
            if entry.url == conn.url && entry.expires_at > now {
                return Ok((entry.client, entry.db_name));
            }
        }

        // 新建连接
        let client = DbClient::new_default(&conn.url)
            .await
            .map_err(|e| ServiceError::DbConnection(e.to_string()))?;
        let entry = CachedDbClient {
            client: client.clone(),
            db_name: db_name.clone(),
            url: conn.url.clone(),
            expires_at: now + DB_CLIENT_CACHE_TTL,
        };
        ctx.db_state().insert_cached_client(db_id.to_string(), entry).await;
        Ok((client, db_name))
    }

    /// 检查指定 db_id 的连接是否为只读
    pub async fn is_read_only_connection(ctx: &ServiceContext, db_id: Option<&str>) -> bool {
        let config = ctx.get_config().await;
        if let Some(id) = db_id {
            return config
                .db_connections
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.is_read_only)
                .unwrap_or(false);
        }
        if let Some(active_id) = config.active_db_id.as_deref() {
            return config
                .db_connections
                .iter()
                .find(|c| c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false);
        }
        false
    }

    // ── WHERE / ORDER 构建 ────────────────────────────────

    fn build_where_clause(
        filters_str: &Option<String>,
    ) -> Result<(String, Vec<String>), ServiceError> {
        let mut where_clause = String::new();
        let mut bindings = Vec::new();

        if let Some(filters_str) = filters_str {
            let filters: Vec<FilterCondition> =
                serde_json::from_str(filters_str).map_err(ServiceError::from)?;
            let mut conditions = Vec::new();
            for f in filters {
                let col = quote_ident_mysql_checked(&f.column)
                    .map_err(ServiceError::BadRequest)?;
                match f.operator.as_str() {
                    "equals" => {
                        conditions.push(format!("{} = ?", col));
                        bindings.push(f.value.clone());
                    }
                    "not_equals" => {
                        conditions.push(format!("{} <> ?", col));
                        bindings.push(f.value.clone());
                    }
                    "contains" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("%{}%", f.value));
                    }
                    "starts_with" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("{}%", f.value));
                    }
                    "ends_with" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("%{}", f.value));
                    }
                    "greater_than" => {
                        conditions.push(format!("{} > ?", col));
                        bindings.push(f.value.clone());
                    }
                    "less_than" => {
                        conditions.push(format!("{} < ?", col));
                        bindings.push(f.value.clone());
                    }
                    "between" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty())
                            .collect();
                        if parts.len() >= 2 {
                            conditions.push(format!("{} BETWEEN ? AND ?", col));
                            bindings.push(parts[0].clone());
                            bindings.push(parts[1].clone());
                        }
                    }
                    "in" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty())
                            .collect();
                        if !parts.is_empty() {
                            let ph = std::iter::repeat("?").take(parts.len()).collect::<Vec<_>>().join(", ");
                            conditions.push(format!("{} IN ({})", col, ph));
                            bindings.extend(parts);
                        }
                    }
                    "not_in" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty())
                            .collect();
                        if !parts.is_empty() {
                            let ph = std::iter::repeat("?").take(parts.len()).collect::<Vec<_>>().join(", ");
                            conditions.push(format!("{} NOT IN ({})", col, ph));
                            bindings.extend(parts);
                        }
                    }
                    "is_null" => conditions.push(format!("{} IS NULL", col)),
                    "is_not_null" => conditions.push(format!("{} IS NOT NULL", col)),
                    _ => {
                        conditions.push(format!("{} = ?", col));
                        bindings.push(f.value.clone());
                    }
                }
            }
            if !conditions.is_empty() {
                where_clause = format!("WHERE {}", conditions.join(" AND "));
            }
        }

        Ok((where_clause, bindings))
    }

    pub(crate) fn build_order_clause(orders_str: &Option<String>) -> Result<String, ServiceError> {
        let mut order_clause = String::new();
        if let Some(orders_str) = orders_str {
            let orders: Vec<OrderCondition> =
                serde_json::from_str(orders_str).map_err(ServiceError::from)?;
            let mut clauses = Vec::new();
            for o in orders {
                let target: String = match o.kind {
                    OrderKind::Column => {
                        let col = o.column.ok_or_else(|| {
                            ServiceError::BadRequest("排序规则缺少 column 字段".into())
                        })?;
                        quote_ident_mysql_checked(&col).map_err(ServiceError::BadRequest)?
                    }
                    OrderKind::Expression => {
                        let expr = o.expression.ok_or_else(|| {
                            ServiceError::BadRequest("排序规则缺少 expression 字段".into())
                        })?;
                        sanitize_sort_expression(&expr)
                            .map_err(|e| ServiceError::BadRequest(format!("非法排序表达式: {}", e)))?;
                        expr
                    }
                };
                let dir = if o.desc { "DESC" } else { "ASC" };
                match o.nulls {
                    OrderNulls::Default => clauses.push(format!("{} {}", target, dir)),
                    OrderNulls::First => clauses.push(format!("{0} IS NULL DESC, {0} {1}", target, dir)),
                    OrderNulls::Last => clauses.push(format!("{0} IS NULL ASC, {0} {1}", target, dir)),
                }
            }
            if !clauses.is_empty() {
                order_clause = format!("ORDER BY {}", clauses.join(", "));
            }
        }
        Ok(order_clause)
    }

    // ── 缓存 key ────────────────────────────────────────

    fn schema_cache_key(db_id: Option<&str>, db_name: &str) -> String {
        match db_id {
            Some(id) => format!("{}::{}", id, db_name),
            None => format!("active::{}", db_name),
        }
    }

    fn table_schema_cache_key(db_id: Option<&str>, db_name: &str, table_name: &str) -> String {
        format!("{}::{}", Self::schema_cache_key(db_id, db_name), table_name)
    }
}

#[cfg(test)]
mod build_order_clause_tests {
    use super::SchemaService;

    fn build(json: &str) -> String {
        SchemaService::build_order_clause(&Some(json.to_string())).unwrap()
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(SchemaService::build_order_clause(&None).unwrap(), "");
        assert_eq!(build("[]"), "");
    }

    #[test]
    fn backward_compatible_legacy_payload() {
        // Old client: only column + desc, no kind/nulls.
        assert_eq!(
            build(r#"[{"column":"id","desc":false}]"#),
            "ORDER BY `id` ASC"
        );
    }

    #[test]
    fn multiple_columns() {
        assert_eq!(
            build(r#"[{"column":"a","desc":false},{"column":"b","desc":true}]"#),
            "ORDER BY `a` ASC, `b` DESC"
        );
    }

    #[test]
    fn nulls_first_asc() {
        assert_eq!(
            build(r#"[{"column":"c","desc":false,"nulls":"first"}]"#),
            "ORDER BY `c` IS NULL DESC, `c` ASC"
        );
    }

    #[test]
    fn nulls_last_desc() {
        assert_eq!(
            build(r#"[{"column":"c","desc":true,"nulls":"last"}]"#),
            "ORDER BY `c` IS NULL ASC, `c` DESC"
        );
    }

    #[test]
    fn expression_rule_accepts_safe_expression() {
        assert_eq!(
            build(r#"[{"kind":"expression","expression":"LENGTH(name)","desc":true}]"#),
            "ORDER BY LENGTH(name) DESC"
        );
    }

    #[test]
    fn expression_rule_rejects_injection() {
        let res = SchemaService::build_order_clause(&Some(
            r#"[{"kind":"expression","expression":"name; DROP TABLE x","desc":false}]"#.to_string(),
        ));
        assert!(res.is_err());
    }

    #[test]
    fn column_rule_missing_column_errors() {
        let res = SchemaService::build_order_clause(&Some(
            r#"[{"kind":"column","desc":false}]"#.to_string(),
        ));
        assert!(res.is_err());
    }

    #[test]
    fn expression_rule_missing_expression_errors() {
        let res = SchemaService::build_order_clause(&Some(
            r#"[{"kind":"expression","desc":false}]"#.to_string(),
        ));
        assert!(res.is_err());
    }
}