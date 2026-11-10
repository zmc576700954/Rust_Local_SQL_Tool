//! MySQL/MariaDB 数据同步的 Plan 阶段：元数据模型、DDL 解析/生成、批次 SQL。
//!
//! 与 PostgreSQL 的 Plan 层共用 `pipeline` 里的作业指纹与拓扑排序；这里只保留
//! MySQL 方言必需的部分：
//! - 用 `SHOW CREATE TABLE` 保留列/生成列/字符集/存储引擎的精确 DDL；
//! - 建表时去掉二级索引与外键，数据灌完后再补；
//! - 文本/HEX/数值三套取值表达式，避免二进制被当 UTF-8 或精度被隐式截断。

use crate::config::DbType;
use crate::error::AppError;
use crate::sql_util::quote_ident_mysql as q;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MySqlColumnSpec {
    pub name: String,
    /// `information_schema.COLUMNS.COLUMN_TYPE`，如 `bigint unsigned`、`decimal(12,2)`。
    pub column_type: String,
    /// 小写基础类型，如 `bigint`、`varchar`、`enum`。
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub extra: String,
    pub generation_expression: Option<String>,
    pub character_set: Option<String>,
    pub collation: Option<String>,
    pub comment: String,
}

impl MySqlColumnSpec {
    pub fn is_generated(&self) -> bool {
        self.extra.to_ascii_lowercase().contains("generated")
    }

    pub fn is_auto_increment(&self) -> bool {
        self.extra.to_ascii_lowercase().contains("auto_increment")
    }

    pub fn is_writable(&self) -> bool {
        !self.is_generated()
    }

    pub fn is_binary(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
        )
    }

    pub fn is_bit(&self) -> bool {
        self.data_type == "bit"
    }

    pub fn is_json(&self) -> bool {
        self.data_type == "json"
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "tinyint"
                | "smallint"
                | "mediumint"
                | "int"
                | "integer"
                | "bigint"
                | "decimal"
                | "numeric"
                | "float"
                | "double"
                | "real"
                | "year"
        )
    }

    pub fn is_integer(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year"
        )
    }

    pub fn is_unsigned(&self) -> bool {
        self.column_type.to_ascii_lowercase().contains("unsigned")
    }

    pub fn is_datetime(&self) -> bool {
        matches!(self.data_type.as_str(), "date" | "datetime" | "timestamp" | "time")
    }

    pub fn is_unsupported_spatial(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "geometry"
                | "point"
                | "linestring"
                | "polygon"
                | "multipoint"
                | "multilinestring"
                | "multipolygon"
                | "geometrycollection"
        )
    }

    /// decimal/numeric 的精确 cast 目标；解析失败时用最大精度兜底。
    pub fn decimal_cast_type(&self) -> String {
        let lower = self.column_type.to_ascii_lowercase();
        if let (Some(open), Some(close)) = (lower.find('('), lower.rfind(')')) {
            if close > open {
                let inside = &self.column_type[open + 1..close];
                let parts = inside.split(',').collect::<Vec<_>>();
                if parts.len() == 2
                    && parts
                        .iter()
                        .all(|p| p.trim().parse::<u32>().is_ok())
                {
                    return format!("DECIMAL({},{})", parts[0].trim(), parts[1].trim());
                }
            }
        }
        "DECIMAL(65,30)".to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MySqlForeignKeySpec {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MySqlIndexSpec {
    pub name: String,
    pub unique: bool,
    /// BTREE / FULLTEXT / SPATIAL / HASH
    pub index_type: String,
    /// (列名, 前缀长度, 排序方向 A/D)
    pub columns: Vec<(String, Option<u32>, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MySqlTablePlan {
    /// 仅表名；MySQL 连接本身已经绑定 database。
    pub display_name: String,
    pub name: String,
    pub columns: Vec<MySqlColumnSpec>,
    pub pk_columns: Vec<String>,
    pub target_exists: bool,
    pub target_columns: Vec<String>,
    pub target_column_types: Vec<(String, String)>,
    pub target_pk_columns: Vec<String>,
    pub target_auto_increment_columns: Vec<String>,
    pub source_count: u64,
    pub foreign_keys: Vec<MySqlForeignKeySpec>,
    pub indexes: Vec<MySqlIndexSpec>,
    /// 建表 DDL：已去掉二级索引与外键。
    pub create_table_ddl: String,
    pub warnings: Vec<String>,
}

impl MySqlTablePlan {
    pub fn qualified(&self) -> String {
        q(&self.name)
    }

    pub fn writable_columns(&self) -> Vec<&MySqlColumnSpec> {
        self.columns.iter().filter(|c| c.is_writable()).collect()
    }

    pub fn pk_indexes_in_writable(&self) -> Result<Vec<usize>, AppError> {
        let writable = self.writable_columns();
        let mut out = Vec::with_capacity(self.pk_columns.len());
        for pk in &self.pk_columns {
            let idx = writable.iter().position(|c| &c.name == pk).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "表 {} 的主键列 {} 是生成列，当前无法按主键分批同步",
                    self.display_name, pk
                ))
            })?;
            out.push(idx);
        }
        Ok(out)
    }

    pub fn pk_types(&self) -> Vec<String> {
        self.pk_columns
            .iter()
            .map(|pk| {
                self.columns
                    .iter()
                    .find(|c| &c.name == pk)
                    .map(|c| c.column_type.clone())
                    .unwrap_or_else(|| "char(255)".to_string())
            })
            .collect()
    }

    pub fn auto_increment_columns(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| c.is_auto_increment())
            .map(|c| c.name.clone())
            .collect()
    }

    pub fn job_key_table(&self) -> crate::sync::pipeline::JobKeyTable {
        crate::sync::pipeline::JobKeyTable {
            display_name: self.display_name.clone(),
            columns: self
                .columns
                .iter()
                .map(|c| crate::sync::pipeline::JobKeyColumn {
                    name: c.name.clone(),
                    type_text: c.column_type.clone(),
                    identity: if c.is_auto_increment() {
                        "auto_increment".to_string()
                    } else {
                        String::new()
                    },
                    generated: if c.is_generated() {
                        "generated".to_string()
                    } else {
                        String::new()
                    },
                })
                .collect(),
            pk_columns: self.pk_columns.clone(),
        }
    }
}

// ── 取值/绑定表达式 ─────────────────────────────────────

/// 源端读取表达式，统一返回字符串（二进制走 HEX，BIT 走无符号十进制）。
pub fn source_select_expr(col: &MySqlColumnSpec) -> String {
    let column = q(&col.name);
    if col.is_binary() {
        format!("HEX({column})")
    } else if col.is_bit() {
        format!("CAST(CAST({column} AS UNSIGNED) AS CHAR)")
    } else {
        format!("CAST({column} AS CHAR)")
    }
}

/// 目标端绑定表达式；`?` 由调用方绑定字符串。
pub fn target_bind_expr(col: &MySqlColumnSpec, target: &DbType) -> String {
    if col.is_binary() {
        return "UNHEX(?)".to_string();
    }
    let data_type = col.data_type.as_str();
    match data_type {
        // MySQL 的 prepared 参数在 CAST(? AS UNSIGNED) 时会先走 DECIMAL 中间态，
        // 超过 i64 的值（如 BIGINT UNSIGNED 的 18446744073709551615）会被截断并
        // 报警告 1292。DECIMAL(20,0) 能精确承载 20 位无符号整数。
        "bit" => "CAST(? AS DECIMAL(20,0))".to_string(),
        "json" => {
            if matches!(target, DbType::MariaDB) {
                "?".to_string()
            } else {
                "CAST(? AS JSON)".to_string()
            }
        }
        "decimal" | "numeric" => format!("CAST(? AS {})", col.decimal_cast_type()),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if col.is_unsigned() {
                "CAST(? AS DECIMAL(20,0))".to_string()
            } else {
                "CAST(? AS SIGNED)".to_string()
            }
        }
        "float" | "double" | "real" => "CAST(? AS DOUBLE)".to_string(),
        "date" => "CAST(? AS DATE)".to_string(),
        // MariaDB 的 CAST 不接受 DATETIME(6)/TIME(6) 精度参数；交给列类型做隐式
        // 转换反而能保留微秒。MySQL 则用显式精度的 CAST。
        "datetime" | "timestamp" => {
            if matches!(target, DbType::MariaDB) {
                "?".to_string()
            } else {
                "CAST(? AS DATETIME(6))".to_string()
            }
        }
        "time" => {
            if matches!(target, DbType::MariaDB) {
                "?".to_string()
            } else {
                "CAST(? AS TIME(6))".to_string()
            }
        }
        _ => "?".to_string(),
    }
}

/// keyset / mirror 比较时的主键绑定表达式。
pub fn pk_bind_expr(col: &MySqlColumnSpec, target: &DbType) -> String {
    if col.is_binary() {
        return "UNHEX(?)".to_string();
    }
    match col.data_type.as_str() {
        // MySQL 的 prepared 参数在 CAST(? AS UNSIGNED) 时会先走 DECIMAL 中间态，
        // 超过 i64 的值（如 BIGINT UNSIGNED 的 18446744073709551615）会被截断并
        // 报警告 1292。DECIMAL(20,0) 能精确承载 20 位无符号整数。
        "bit" => "CAST(? AS DECIMAL(20,0))".to_string(),
        "decimal" | "numeric" => format!("CAST(? AS {})", col.decimal_cast_type()),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if col.is_unsigned() {
                "CAST(? AS DECIMAL(20,0))".to_string()
            } else {
                "CAST(? AS SIGNED)".to_string()
            }
        }
        "date" => "CAST(? AS DATE)".to_string(),
        "datetime" | "timestamp" => {
            if matches!(target, DbType::MariaDB) {
                "?".to_string()
            } else {
                "CAST(? AS DATETIME(6))".to_string()
            }
        }
        "time" => {
            if matches!(target, DbType::MariaDB) {
                "?".to_string()
            } else {
                "CAST(? AS TIME(6))".to_string()
            }
        }
        _ => "?".to_string(),
    }
}

pub fn order_by(pk_columns: &[String]) -> String {
    pk_columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
}

fn row_ref(columns: &[String]) -> String {
    if columns.len() == 1 {
        q(&columns[0])
    } else {
        format!(
            "({})",
            columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
        )
    }
}

fn pk_select_list(plan: &MySqlTablePlan) -> String {
    plan.pk_columns
        .iter()
        .filter_map(|pk| plan.columns.iter().find(|c| &c.name == pk))
        .map(|c| format!("{} AS {}", source_select_expr(c), q(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pk_bind_list(plan: &MySqlTablePlan, target: &DbType) -> String {
    let exprs = plan
        .pk_columns
        .iter()
        .filter_map(|pk| plan.columns.iter().find(|c| &c.name == pk))
        .map(|c| pk_bind_expr(c, target))
        .collect::<Vec<_>>();
    exprs.join(", ")
}

fn all_select_list(plan: &MySqlTablePlan) -> String {
    plan.writable_columns()
        .iter()
        .map(|c| format!("{} AS {}", source_select_expr(c), q(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 分批读取（按主键 keyset）。`?` 依次是主键游标、LIMIT。
pub fn batch_select_sql(plan: &MySqlTablePlan, has_cursor: bool, target: &DbType) -> String {
    let mut sql = format!(
        "SELECT {} FROM {}",
        all_select_list(plan),
        plan.qualified()
    );
    if has_cursor && !plan.pk_columns.is_empty() {
        sql.push_str(&format!(
            " WHERE {} > ({})",
            row_ref(&plan.pk_columns),
            pk_bind_list(plan, target)
        ));
    }
    sql.push_str(&format!(" ORDER BY {}", order_by(&plan.pk_columns)));
    sql.push_str(" LIMIT ?");
    sql
}

/// 无主键表的整表读取。
pub fn full_select_sql(plan: &MySqlTablePlan) -> String {
    format!("SELECT {} FROM {}", all_select_list(plan), plan.qualified())
}

/// 目标端逐行校验/镜像比较用的主键读取。
pub fn target_pk_select_sql(
    plan: &MySqlTablePlan,
    target: &DbType,
    has_lower: bool,
    has_upper: bool,
) -> String {
    let mut sql = format!(
        "SELECT {} FROM {}",
        pk_select_list(plan),
        plan.qualified()
    );
    let mut conditions = Vec::new();
    if has_lower {
        conditions.push(format!(
            "{} > ({})",
            row_ref(&plan.pk_columns),
            pk_bind_list(plan, target)
        ));
    }
    if has_upper {
        conditions.push(format!(
            "{} <= ({})",
            row_ref(&plan.pk_columns),
            pk_bind_list(plan, target)
        ));
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(&format!(" ORDER BY {}", order_by(&plan.pk_columns)));
    sql
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertDialect {
    /// MySQL 8.0.19+：`VALUES (...) AS new ON DUPLICATE KEY UPDATE c = new.c`
    AliasNew,
    /// MariaDB / MySQL < 8.0.19：`ON DUPLICATE KEY UPDATE c = VALUES(c)`
    ValuesFunction,
}

/// 用服务端 `VERSION()` 的结果选方言，避免只凭 URL/声明类型硬猜。
pub fn upsert_dialect(target: &DbType, version: &str) -> UpsertDialect {
    let lower = version.to_ascii_lowercase();
    if matches!(target, DbType::MariaDB) || lower.contains("mariadb") {
        return UpsertDialect::ValuesFunction;
    }
    let mut parts = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<u64>().ok());
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    if (major, minor, patch) >= (8, 0, 19) {
        UpsertDialect::AliasNew
    } else {
        UpsertDialect::ValuesFunction
    }
}

/// 多行 upsert。`row_count` 决定 VALUES 行数；绑定顺序与 `writable_columns` 一致。
pub fn upsert_sql(
    plan: &MySqlTablePlan,
    row_count: usize,
    target: &DbType,
    dialect: UpsertDialect,
) -> String {
    let writable = plan.writable_columns();
    let column_list = writable
        .iter()
        .map(|c| q(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let cells = writable
            .iter()
            .map(|c| target_bind_expr(c, target))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(format!("({cells})"));
    }
    let values = rows.join(", ");

    if plan.pk_columns.is_empty() {
        // 无主键表只能普通插入；幂等性由引擎在结果里如实声明。
        return format!(
            "INSERT INTO {} ({}) VALUES {}",
            plan.qualified(),
            column_list,
            values
        );
    }

    let non_pk = writable
        .iter()
        .filter(|c| !plan.pk_columns.contains(&c.name))
        .map(|c| c.name.clone())
        .collect::<Vec<_>>();
    let update = if non_pk.is_empty() {
        let first = q(&plan.pk_columns[0]);
        format!("{first} = {first}")
    } else {
        match dialect {
            UpsertDialect::AliasNew => non_pk
                .iter()
                .map(|c| format!("{} = new.{}", q(c), q(c)))
                .collect::<Vec<_>>()
                .join(", "),
            UpsertDialect::ValuesFunction => non_pk
                .iter()
                .map(|c| format!("{} = VALUES({})", q(c), q(c)))
                .collect::<Vec<_>>()
                .join(", "),
        }
    };
    match dialect {
        UpsertDialect::AliasNew => format!(
            "INSERT INTO {} ({}) VALUES {} AS new ON DUPLICATE KEY UPDATE {}",
            plan.qualified(),
            column_list,
            values,
            update
        ),
        UpsertDialect::ValuesFunction => format!(
            "INSERT INTO {} ({}) VALUES {} ON DUPLICATE KEY UPDATE {}",
            plan.qualified(),
            column_list,
            values,
            update
        ),
    }
}

pub fn delete_by_pk_sql(plan: &MySqlTablePlan, target: &DbType) -> String {
    format!(
        "DELETE FROM {} WHERE {} = ({})",
        plan.qualified(),
        row_ref(&plan.pk_columns),
        pk_bind_list(plan, target)
    )
}

pub fn delete_after_pk_sql(plan: &MySqlTablePlan, target: &DbType) -> String {
    format!(
        "DELETE FROM {} WHERE {} > ({})",
        plan.qualified(),
        row_ref(&plan.pk_columns),
        pk_bind_list(plan, target)
    )
}

pub fn delete_all_sql(plan: &MySqlTablePlan) -> String {
    format!("DELETE FROM {}", plan.qualified())
}

// ── DDL ────────────────────────────────────────────────

/// 从 `SHOW CREATE TABLE` 中去掉二级索引与外键，只保留列、主键与 CHECK。
pub fn strip_secondary_indexes_and_fks(ddl: &str) -> Result<String, AppError> {
    let open = ddl
        .find('(')
        .ok_or_else(|| AppError::InternalError("无法解析 SHOW CREATE TABLE：缺少 '('".to_string()))?;
    let close = find_matching_paren(ddl, open).ok_or_else(|| {
        AppError::InternalError("无法解析 SHOW CREATE TABLE：括号不匹配".to_string())
    })?;
    let prefix = &ddl[..=open];
    let suffix = &ddl[close..];
    let body = &ddl[open + 1..close];
    let parts = split_top_level(body);
    let mut kept = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        let drop = upper.starts_with("KEY ")
            || upper.starts_with("UNIQUE KEY ")
            || upper.starts_with("FULLTEXT KEY ")
            || upper.starts_with("SPATIAL KEY ")
            || upper.starts_with("FOREIGN KEY ")
            || (upper.starts_with("CONSTRAINT ") && upper.contains("FOREIGN KEY"));
        if !drop {
            kept.push(trimmed.to_string());
        }
    }
    if kept.is_empty() {
        return Err(AppError::InternalError(
            "无法解析 SHOW CREATE TABLE：列定义为空".to_string(),
        ));
    }
    Ok(format!(
        "{} \n  {}\n{}",
        prefix,
        kept.join(",\n  "),
        suffix
    ))
}

fn find_matching_paren(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_backtick = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for i in open..bytes.len() {
        let ch = bytes[i] as char;
        if escaped {
            escaped = false;
            continue;
        }
        if in_single {
            if ch == '\\' {
                escaped = true;
            } else if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_backtick {
            if ch == '`' {
                in_backtick = false;
            }
            continue;
        }
        match ch {
            '`' => in_backtick = true,
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_backtick = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for i in 0..bytes.len() {
        let ch = bytes[i] as char;
        if escaped {
            escaped = false;
            continue;
        }
        if in_single {
            if ch == '\\' {
                escaped = true;
            } else if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_backtick {
            if ch == '`' {
                in_backtick = false;
            }
            continue;
        }
        match ch {
            '`' => in_backtick = true,
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(body[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(body[start..].to_string());
    out
}

pub fn create_index_sql(plan: &MySqlTablePlan, index: &MySqlIndexSpec) -> String {
    let kind = match index.index_type.to_ascii_uppercase().as_str() {
        "FULLTEXT" => "FULLTEXT INDEX",
        "SPATIAL" => "SPATIAL INDEX",
        _ if index.unique => "UNIQUE INDEX",
        _ => "INDEX",
    };
    let cols = index
        .columns
        .iter()
        .map(|(name, prefix, _)| match prefix {
            Some(len) => format!("{}({len})", q(name)),
            None => q(name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {} {} ON {} ({})",
        kind,
        q(&index.name),
        plan.qualified(),
        cols
    )
}

pub fn add_foreign_key_sql(plan: &MySqlTablePlan, fk: &MySqlForeignKeySpec) -> String {
    let ref_table = if fk.referenced_schema.is_empty() {
        q(&fk.referenced_table)
    } else {
        format!("{}.{}", q(&fk.referenced_schema), q(&fk.referenced_table))
    };
    let cols = fk.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let ref_cols = fk
        .referenced_columns
        .iter()
        .map(|c| q(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        plan.qualified(),
        q(&fk.name),
        cols,
        ref_table,
        ref_cols
    );
    if !fk.on_delete.is_empty() && fk.on_delete.to_ascii_uppercase() != "NO ACTION" {
        sql.push_str(&format!(" ON DELETE {}", fk.on_delete));
    }
    if !fk.on_update.is_empty() && fk.on_update.to_ascii_uppercase() != "NO ACTION" {
        sql.push_str(&format!(" ON UPDATE {}", fk.on_update));
    }
    sql
}

pub fn validate_plan(plan: &MySqlTablePlan, allow_no_pk: bool) -> Result<(), AppError> {
    if plan.pk_columns.is_empty() && !allow_no_pk {
        return Err(AppError::BadRequest(format!(
            "表 {} 没有主键，无法安全地分批同步/断点续跑；如确认要整表复制，请显式设置 allow_no_pk=true（该表不具备幂等与分块哈希校验）",
            plan.display_name
        )));
    }
    for pk in &plan.pk_columns {
        let Some(col) = plan.columns.iter().find(|c| &c.name == pk) else {
            return Err(AppError::BadRequest(format!(
                "表 {} 的主键列 {} 在列元数据中不存在",
                plan.display_name, pk
            )));
        };
        if col.is_generated() {
            return Err(AppError::BadRequest(format!(
                "表 {} 的主键列 {} 是生成列，当前无法按主键分批同步",
                plan.display_name, pk
            )));
        }
    }
    if plan.target_exists {
        for col in plan.writable_columns() {
            if !plan.target_columns.contains(&col.name) {
                return Err(AppError::BadRequest(format!(
                    "目标表 {} 缺少源列 {}，无法写入；请先做结构同步",
                    plan.display_name, col.name
                )));
            }
        }
        if !plan.pk_columns.is_empty() && plan.target_pk_columns.is_empty() {
            return Err(AppError::BadRequest(format!(
                "目标表 {} 没有主键，ON DUPLICATE KEY UPDATE 无法保证幂等，拒绝同步；请先补主键或改用 mirror/手工方案",
                plan.display_name
            )));
        }
        if !plan.pk_columns.is_empty()
            && !plan.target_pk_columns.is_empty()
            && plan.pk_columns != plan.target_pk_columns
        {
            return Err(AppError::BadRequest(format!(
                "表 {} 的源/目标主键不一致（源 {:?}，目标 {:?}）",
                plan.display_name, plan.pk_columns, plan.target_pk_columns
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, column_type: &str, data_type: &str) -> MySqlColumnSpec {
        MySqlColumnSpec {
            name: name.to_string(),
            column_type: column_type.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
            default: None,
            extra: String::new(),
            generation_expression: None,
            character_set: None,
            collation: None,
            comment: String::new(),
        }
    }

    fn plan(columns: Vec<MySqlColumnSpec>, pk: Vec<&str>) -> MySqlTablePlan {
        MySqlTablePlan {
            display_name: "t".to_string(),
            name: "t".to_string(),
            columns,
            pk_columns: pk.into_iter().map(|s| s.to_string()).collect(),
            target_exists: false,
            target_columns: vec![],
            target_column_types: vec![],
            target_pk_columns: vec![],
            target_auto_increment_columns: vec![],
            source_count: 0,
            foreign_keys: vec![],
            indexes: vec![],
            create_table_ddl: String::new(),
            warnings: vec![],
        }
    }

    #[test]
    fn upsert_dialect_uses_server_version_and_mariadb_type() {
        assert_eq!(
            upsert_dialect(&DbType::MySQL, "8.0.36"),
            UpsertDialect::AliasNew
        );
        assert_eq!(
            upsert_dialect(&DbType::MySQL, "8.0.18"),
            UpsertDialect::ValuesFunction
        );
        assert_eq!(
            upsert_dialect(&DbType::MariaDB, "10.11.6-MariaDB"),
            UpsertDialect::ValuesFunction
        );
        assert_eq!(
            upsert_dialect(&DbType::MySQL, "10.11.6-MariaDB"),
            UpsertDialect::ValuesFunction
        );
    }

    #[test]
    fn upsert_sql_matches_dialect() {
        let p = plan(
            vec![col("id", "int", "int"), col("v", "varchar(10)", "varchar")],
            vec!["id"],
        );
        let alias = upsert_sql(&p, 2, &DbType::MySQL, UpsertDialect::AliasNew);
        assert!(alias.contains("AS new ON DUPLICATE KEY UPDATE `v` = new.`v`"));
        let values = upsert_sql(&p, 2, &DbType::MariaDB, UpsertDialect::ValuesFunction);
        assert!(values.contains("ON DUPLICATE KEY UPDATE `v` = VALUES(`v`)"));
        assert!(!values.contains("AS new"));
    }

    #[test]
    fn binary_and_bit_use_hex_and_unsigned() {
        let b = col("b", "blob", "blob");
        assert_eq!(source_select_expr(&b), "HEX(`b`)");
        assert_eq!(target_bind_expr(&b, &DbType::MySQL), "UNHEX(?)");
        let bit = col("f", "bit(8)", "bit");
        assert_eq!(
            source_select_expr(&bit),
            "CAST(CAST(`f` AS UNSIGNED) AS CHAR)"
        );
        assert_eq!(
            target_bind_expr(&bit, &DbType::MySQL),
            // BIT(64) 的最大值超出 i64，用 DECIMAL(20,0) 避免 prepared 参数被截断。
            "CAST(? AS DECIMAL(20,0))"
        );
    }

    #[test]
    fn decimal_cast_keeps_precision() {
        let d = col("amount", "decimal(20,6)", "decimal");
        assert_eq!(target_bind_expr(&d, &DbType::MySQL), "CAST(? AS DECIMAL(20,6))");
        let plain = col("n", "decimal", "decimal");
        assert_eq!(target_bind_expr(&plain, &DbType::MySQL), "CAST(? AS DECIMAL(65,30))");
    }

    #[test]
    fn json_cast_is_mariadb_compatible() {
        let j = col("payload", "json", "json");
        assert_eq!(target_bind_expr(&j, &DbType::MySQL), "CAST(? AS JSON)");
        assert_eq!(target_bind_expr(&j, &DbType::MariaDB), "?");
    }

    #[test]
    fn strip_ddl_removes_secondary_indexes_and_fks_but_keeps_columns_and_pk() {
        let ddl = "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `v` varchar(10) DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `u` (`v`),\n  KEY `k` (`v`),\n  CONSTRAINT `fk` FOREIGN KEY (`id`) REFERENCES `p` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        let stripped = strip_secondary_indexes_and_fks(ddl).unwrap();
        assert!(stripped.contains("`id` int NOT NULL AUTO_INCREMENT"));
        assert!(stripped.contains("`v` varchar(10) DEFAULT NULL"));
        assert!(stripped.contains("PRIMARY KEY (`id`)"));
        assert!(!stripped.contains("UNIQUE KEY"));
        assert!(!stripped.contains("KEY `k`"));
        assert!(!stripped.contains("FOREIGN KEY"));
        assert!(stripped.contains("ENGINE=InnoDB"));
    }

    #[test]
    fn strip_ddl_handles_enum_commas_and_check_constraints() {
        let ddl = "CREATE TABLE `t` (\n  `s` enum('a','b,c') NOT NULL,\n  PRIMARY KEY (`s`),\n  CONSTRAINT `chk` CHECK ((`s` <> '')),\n  KEY `k` (`s`)\n) ENGINE=InnoDB";
        let stripped = strip_secondary_indexes_and_fks(ddl).unwrap();
        assert!(stripped.contains("enum('a','b,c')"));
        assert!(stripped.contains("CHECK"));
        assert!(!stripped.contains("KEY `k`"));
    }
}
