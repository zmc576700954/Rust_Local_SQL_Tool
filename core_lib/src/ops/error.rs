//! ops 模块统一错误类型。
//!
//! 所有错误都必须能映射成 HTTP 语义，便于 web-server 直接转成 AppError。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpsError {
    #[error("计划无效: {0}")]
    InvalidPlan(String),
    #[error("校验失败: {0}")]
    Validation(String),
    #[error("审批拒绝: {0}")]
    Approval(String),
    #[error("策略拒绝: {0}")]
    Policy(String),
    #[error("未找到: {0}")]
    NotFound(String),
    #[error("数据库错误: {0}")]
    Db(String),
    #[error("文件错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("序列化错误: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<sqlx::Error> for OpsError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err.to_string())
    }
}

impl From<crate::db::client::DbError> for OpsError {
    fn from(err: crate::db::client::DbError) -> Self {
        Self::Db(err.to_string())
    }
}

impl From<OpsError> for crate::error::AppError {
    fn from(err: OpsError) -> Self {
        match err {
            OpsError::InvalidPlan(msg)
            | OpsError::Validation(msg)
            | OpsError::Internal(msg) => crate::error::AppError::BadRequest(msg),
            OpsError::Serde(err) => crate::error::AppError::BadRequest(err.to_string()),
            OpsError::Approval(msg) | OpsError::Policy(msg) => {
                crate::error::AppError::Forbidden(msg)
            }
            OpsError::NotFound(msg) => crate::error::AppError::NotFound(msg),
            OpsError::Db(msg) => crate::error::AppError::DbQueryError {
                message: msg,
                code: None,
            },
            OpsError::Io(err) => crate::error::AppError::InternalError(err.to_string()),
        }
    }
}

impl OpsError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidPlan(_) | Self::Validation(_) | Self::Serde(_) => 400,
            Self::Approval(_) | Self::Policy(_) => 403,
            Self::NotFound(_) => 404,
            Self::Db(_) => 502,
            Self::Io(_) | Self::Internal(_) => 500,
        }
    }
}
