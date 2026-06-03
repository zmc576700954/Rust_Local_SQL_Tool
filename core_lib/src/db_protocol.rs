use crate::config::DbType;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedQueryRequest {
    pub statement: String,
    pub database: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedQueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub affected_rows: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedTableRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedColumn {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedTableSchema {
    pub table: UnifiedTableRef,
    pub columns: Vec<UnifiedColumn>,
}

pub trait UnifiedQueryEngine: Send + Sync {
    fn db_type(&self) -> DbType;
    fn execute<'a>(
        &'a self,
        req: UnifiedQueryRequest,
    ) -> BoxFuture<'a, Result<UnifiedQueryResult, AppError>>;
}

pub trait UnifiedMetadataProvider: Send + Sync {
    fn db_type(&self) -> DbType;
    fn list_databases<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>, AppError>>;
    fn list_tables<'a>(
        &'a self,
        database: &str,
    ) -> BoxFuture<'a, Result<Vec<UnifiedTableRef>, AppError>>;
    fn get_table_schema<'a>(
        &'a self,
        table: UnifiedTableRef,
    ) -> BoxFuture<'a, Result<UnifiedTableSchema, AppError>>;
}

pub trait UnifiedImportExport: Send + Sync {
    fn db_type(&self) -> DbType;
    fn export_table<'a>(
        &'a self,
        table: UnifiedTableRef,
    ) -> BoxFuture<'a, Result<Vec<u8>, AppError>>;
    fn import_table<'a>(
        &'a self,
        table: UnifiedTableRef,
        payload: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), AppError>>;
}
