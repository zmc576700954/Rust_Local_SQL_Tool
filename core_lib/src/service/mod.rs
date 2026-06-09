//! Service Layer — 统一业务逻辑入口
//!
//! 将 web-server 和 src-tauri 的共享业务逻辑统一下沉到此模块，
//! 两者仅作为接入层薄壳（参数转换 → Service 调用 → 响应转换）。

pub mod error;
pub mod context;
pub mod row_codec;
pub mod schema;
pub mod workbench;
pub mod config;
pub mod crud;
pub mod ai;

// 后续 Step 逐步添加的模块
// pub mod db_cache;
// pub mod transfer;
// pub mod sync;
// pub mod perf;

pub use error::ServiceError;
pub use context::ServiceContext;
pub use schema::SchemaService;
pub use workbench::WorkbenchService;
pub use config::ConfigService;
pub use crud::CrudService;
pub use ai::AiService;