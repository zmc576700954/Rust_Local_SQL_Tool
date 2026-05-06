use crate::schema::{ColumnInfo, SchemaResponse, TableWithDetails};
use crate::schema_ext::IndexInfo;
use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

pub struct OfflineParser;

impl OfflineParser {
    pub fn parse_sql(sql: &str) -> Result<SchemaResponse, String> {
        let dialect = MySqlDialect {};
        let statements =
            Parser::parse_sql(&dialect, sql).map_err(|e| format!("Failed to parse SQL: {}", e))?;

        let mut tables = Vec::new();
        let parsed_db_name = "offline_db".to_string();

        for statement in statements {
            if let Statement::CreateTable(ct) = statement {
                let table_name = ct.name.to_string().replace("`", "");

                let mut parsed_columns = Vec::new();
                let mut parsed_indexes = Vec::new();

                for col in ct.columns {
                    let is_not_null = col.options.iter().any(|o| {
                        let opt_str = o.option.to_string().to_uppercase();
                        opt_str.contains("NOT NULL")
                    });

                    let is_primary = col.options.iter().any(|o| {
                        let opt_str = o.option.to_string().to_uppercase();
                        opt_str.contains("PRIMARY KEY")
                    });

                    let is_nullable = if is_not_null || is_primary {
                        "NO"
                    } else {
                        "YES"
                    };
                    let column_key = if is_primary { "PRI" } else { "" };

                    let col_name = col
                        .name
                        .to_string()
                        .replace("`", "")
                        .replace("'", "")
                        .replace("\"", "");

                    if is_primary {
                        parsed_indexes.push(IndexInfo {
                            index_name: "PRIMARY".to_string(),
                            column_name: col_name.clone(),
                            non_unique: false,
                            index_type: "BTREE".to_string(),
                        });
                    }

                    parsed_columns.push(ColumnInfo {
                        column_name: col_name,
                        data_type: col.data_type.to_string().to_uppercase(),
                        column_type: col.data_type.to_string().to_uppercase(),
                        is_nullable: is_nullable.to_string(),
                        column_comment: None,
                        column_key: column_key.to_string(),
                        column_default: None,
                        extra: "".to_string(),
                    });
                }

                tables.push(TableWithDetails {
                    table_name,
                    columns: parsed_columns,
                    indexes: parsed_indexes,
                    foreign_keys: vec![],
                });
            }
        }

        Ok(SchemaResponse {
            db_name: parsed_db_name,
            tables,
            views: vec![], // Add view parsing later if needed
        })
    }
}
