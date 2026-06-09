//! ConfigService — 配置读取 / 更新 / 级联刷新
//!
//! 从 web-server/src/main.rs get_config / update_config 提取核心业务逻辑。
//! Service 层不含 axum / tauri 依赖。

use std::collections::HashMap;

use crate::config::AppConfig;
use crate::db::DbClient;
use crate::service::context::ServiceContext;
use crate::service::error::ServiceError;
use crate::service::schema::SchemaService;

// ── 参数 / 响应类型 ─────────────────────────────────────

/// 配置更新请求（客户端提交的新配置）
pub struct UpdateConfigParams {
    pub new_config: AppConfig,
}

// ── ConfigService ───────────────────────────────────────

pub struct ConfigService;

impl ConfigService {
    /// 获取配置（脱敏后 + 标记位）
    pub async fn get_config(ctx: &ServiceContext) -> serde_json::Value {
        let config = ctx.get_config().await;
        Self::config_for_client(&config)
    }

    /// 更新配置（归一化 + 合并密钥 + 保存 + 级联刷新）
    pub async fn update_config(
        ctx: &ServiceContext,
        params: UpdateConfigParams,
    ) -> Result<serde_json::Value, ServiceError> {
        let prev_config = ctx.get_config().await;
        let mut new_config = params.new_config.normalize();
        new_config.merge_secrets_from(&prev_config);

        // 保存到文件
        new_config
            .save()
            .await
            .map_err(|e| ServiceError::ConfigInvalid(e.to_string()))?;

        // 更新内存状态
        ctx.update_config(new_config.clone()).await;
        ctx.db_state().clear_cached_clients().await;
        SchemaService::clear_metadata_caches(ctx).await;

        // 如果活跃 DB URL 变化，重新初始化连接
        if let Some(url) = new_config.get_active_db_url() {
            let db_type = new_config.get_active_db_type_enum();
            match DbClient::new(&url, &new_config.pool_config, &db_type).await {
                Ok(client) => {
                    // 关闭旧连接
                    if let Some(old) = ctx.db_state().clear_active_client().await {
                        old.pool.close().await;
                    }
                    ctx.db_state().set_active_client(client).await;
                }
                Err(e) => return Err(ServiceError::BadRequest(format!("DB connection failed: {}", e))),
            }
        }

        Ok(Self::config_for_client(&new_config))
    }

    // ── 内部辅助 ──────────────────────────────────────────

    /// 为客户端准备脱敏配置（redacted + api_key_set / token_pool_set 标记）
    fn config_for_client(raw: &AppConfig) -> serde_json::Value {
        let api_key_set = raw.api_key.as_ref().is_some_and(|s| !s.is_empty());
        let token_pool_set = !raw.token_pool.is_empty();
        let mut profile_flags: HashMap<String, (bool, bool)> = HashMap::new();
        for p in &raw.ai_profiles {
            let p_api_key_set = p.api_key.as_ref().is_some_and(|s| !s.is_empty());
            let p_token_pool_set = !p.pool.tokens.is_empty();
            profile_flags.insert(p.id.clone(), (p_api_key_set, p_token_pool_set));
        }

        let redacted = raw.redacted_for_client();
        let mut v = serde_json::to_value(redacted).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("api_key_set".to_string(), serde_json::Value::Bool(api_key_set));
            obj.insert("token_pool_set".to_string(), serde_json::Value::Bool(token_pool_set));
            if let Some(arr) = obj.get_mut("ai_profiles").and_then(|x| x.as_array_mut()) {
                for item in arr {
                    if let Some(pobj) = item.as_object_mut() {
                        let id = pobj
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        if let Some((k, t)) = profile_flags.get(&id).copied() {
                            pobj.insert("api_key_set".to_string(), serde_json::Value::Bool(k));
                            pobj.insert("token_pool_set".to_string(), serde_json::Value::Bool(t));
                        }
                    }
                }
            }
        }
        v
    }
}