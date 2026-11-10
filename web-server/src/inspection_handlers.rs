//! 定期巡检路由：计划设置、手动运行、最近结果、历史、diff 与告警。
//!
//! 写操作统一要求本机 ops 令牌；巡检本身只读，不会调用允许 apply 的执行通道。

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use core_lib::config::DbType;
use core_lib::db::client::DbClient;
use core_lib::ops::inspection::alert::{
    deliver_with_retry, NotificationDispatcher, RetryPolicy,
};
use core_lib::ops::inspection::diff::compare_snapshots;
use core_lib::ops::inspection::notify::{
    EmailNotificationSettings, NotificationSecrets, NotificationSettings, SCOPE_NOTE,
    WebhookNotificationSettings,
};
use core_lib::ops::inspection::presets::{
    all_check_ids, resolve_check_ids, CHECK_DESCRIPTORS, presets,
};
use core_lib::ops::inspection::redact::{
    scrub_alert, scrub_diff, scrub_json, scrub_run_result, scrub_snapshot, scrub_text,
};
use core_lib::ops::inspection::runner::run_inspection;
use core_lib::ops::inspection::scheduler::{compute_next_run, validate_schedule};
use core_lib::ops::inspection::types::{
    now_ts, AlertRecord, AlertState, ChannelDelivery, DeliveryStatus, InspectionRunResult,
    InspectionSchedule, InspectionSnapshot, InspectionThresholds, ScheduleMode, SnapshotDiff,
    SnapshotSummary, CHECK_UNUSED_INDEX,
};
use core_lib::ops::types::hash_json;

use core_lib::error::AppError;
use crate::ops_handlers::{audit_event, authorize_write, build_ctx};
use crate::state::AppState;

// ── 目标数据库解析 ──────────────────────────────────────────

#[derive(Debug, Clone)]
struct DbTarget {
    /// 保存连接的真实 id；legacy 连接为 None（沿用当前激活连接）。
    id: Option<String>,
    /// 用于快照查询的 db_id 键（legacy 为空字符串）。
    id_key: String,
    display_name: String,
    db_name: String,
    db_type: DbType,
    fingerprint: String,
}

async fn resolve_target(
    state: &AppState,
    requested: Option<&str>,
) -> Result<DbTarget, AppError> {
    let config = state.config.read().await;
    let connection = match requested.filter(|id| !id.trim().is_empty()) {
        Some(id) => config
            .db_connections
            .iter()
            .find(|connection| connection.id == id)
            .cloned()
            .ok_or_else(|| AppError::BadRequest(format!("数据库连接不存在: {id}")))?,
        None => config.get_active_db_connection().ok_or_else(|| {
            AppError::BadRequest("尚未激活数据库连接，请先在连接管理中激活".into())
        })?,
    };
    let saved_id = if connection.id == "legacy" {
        None
    } else {
        Some(connection.id.clone())
    };
    let id_key = saved_id.clone().unwrap_or_default();
    let db_type = connection
        .db_type
        .clone()
        .or_else(|| DbType::from_url(&connection.url))
        .unwrap_or(DbType::MySQL);
    if !matches!(db_type, DbType::MySQL | DbType::MariaDB | DbType::PostgreSQL) {
        return Err(AppError::BadRequest(format!(
            "定期巡检支持 MySQL / MariaDB / PostgreSQL；当前连接为 {}，SQLite 仍为实验性，请切换连接后再使用该功能。",
            db_type.display_name()
        )));
    }
    let db_name = DbClient::extract_db_name(&connection.url).unwrap_or_default();
    let fingerprint = hash_json(&json!({
        "db_type": format!("{db_type:?}"),
        "db_id": id_key,
        "db_name": db_name,
    }));
    Ok(DbTarget {
        id: saved_id,
        id_key,
        display_name: connection.name,
        db_name,
        db_type,
        fingerprint,
    })
}

fn target_json(target: &DbTarget) -> serde_json::Value {
    let mut value = json!({
        "db_id": target.id.clone(),
        "db_name": target.db_name.clone(),
        "display_name": target.display_name.clone(),
        "db_type": format!("{:?}", target.db_type),
        "db_fingerprint": target.fingerprint.clone(),
    });
    // 连接名称可能被用户写成 DSN 形式，展示前统一按形态剔除。
    scrub_json(&mut value, &[]);
    value
}

// ── 元信息 / 计划 ───────────────────────────────────────────

pub(crate) async fn ops_inspection_presets() -> Json<serde_json::Value> {
    Json(json!({
        "presets": presets(),
        "checks": CHECK_DESCRIPTORS.iter().map(|descriptor| json!({
            "id": descriptor.id,
            "title": descriptor.title,
            "rule": descriptor.rule,
        })).collect::<Vec<_>>(),
        "scope": SCOPE_NOTE,
    }))
}

#[derive(Serialize)]
pub(crate) struct ScheduleResponse {
    schedule: InspectionSchedule,
    presets: Vec<core_lib::ops::inspection::types::InspectionPreset>,
    checks: serde_json::Value,
    target: Option<serde_json::Value>,
    notification_channels: Vec<String>,
    scope_note: String,
}

pub(crate) async fn ops_inspection_get_schedule(
    State(state): State<AppState>,
) -> Result<Json<ScheduleResponse>, AppError> {
    let schedule = state.ops.inspections.read().await.schedule().clone();
    let channels = {
        let store = state.ops.inspections.read().await;
        let settings = store.notification_settings();
        let dispatcher = NotificationDispatcher::from_settings(
            &settings.email,
            &settings.webhook,
            store.notification_secrets(),
        );
        dispatcher
            .channel_ids()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let target = if schedule.db_id.trim().is_empty() {
        None
    } else {
        resolve_target(&state, Some(&schedule.db_id))
            .await
            .ok()
            .map(|target| target_json(&target))
    };
    Ok(Json(ScheduleResponse {
        schedule,
        presets: presets(),
        checks: json!(CHECK_DESCRIPTORS
            .iter()
            .map(|descriptor| json!({
                "id": descriptor.id,
                "title": descriptor.title,
                "rule": descriptor.rule,
            }))
            .collect::<Vec<_>>()),
        target,
        notification_channels: channels,
        scope_note: SCOPE_NOTE.into(),
    }))
}

#[derive(Deserialize)]
pub(crate) struct SchedulePayload {
    enabled: bool,
    mode: ScheduleMode,
    #[serde(default = "default_interval_minutes")]
    interval_minutes: u64,
    #[serde(default = "default_cron_expr")]
    cron_expr: String,
    preset_id: String,
    #[serde(default)]
    check_ids: Option<Vec<String>>,
    #[serde(default)]
    db_id: Option<String>,
}

fn default_interval_minutes() -> u64 {
    1440
}

fn default_cron_expr() -> String {
    "0 3 * * *".into()
}

pub(crate) async fn ops_inspection_save_schedule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SchedulePayload>,
) -> Result<Json<serde_json::Value>, AppError> {
    authorize_write(&state, &headers)?;

    let explicit = payload.check_ids.unwrap_or_default();
    let check_ids = resolve_check_ids(&payload.preset_id, &explicit).ok_or_else(|| {
        AppError::BadRequest(format!(
            "未知巡检预设或自定义检查项为空: {}",
            payload.preset_id
        ))
    })?;

    let target = resolve_target(&state, payload.db_id.as_deref()).await?;
    let target_id = target.id.clone().ok_or_else(|| {
        AppError::BadRequest("该连接没有保存 id，无法作为巡检计划目标；请先在连接管理中保存并激活".into())
    })?;

    let existing = state.ops.inspections.read().await.schedule().clone();
    let mut schedule = InspectionSchedule {
        enabled: payload.enabled,
        mode: payload.mode,
        interval_minutes: payload.interval_minutes,
        cron_expr: payload.cron_expr,
        preset_id: payload.preset_id,
        check_ids,
        db_id: target_id,
        db_name: target.display_name.clone(),
        updated_at: now_ts(),
        last_run_at: existing.last_run_at,
        next_run_at: None,
        last_error: None,
    };
    validate_schedule(&schedule).map_err(AppError::BadRequest)?;
    if schedule.enabled {
        let now = now_ts();
        schedule.next_run_at = Some(
            compute_next_run(&schedule, now)
                .map_err(|err| AppError::BadRequest(format!("无法计算下一次巡检时间: {err}")))?,
        );
    }

    state
        .ops
        .inspections
        .write()
        .await
        .save_schedule(schedule.clone())?;
    audit_event(
        &state,
        "inspection_schedule_saved",
        "ok",
        json!({
            "enabled": schedule.enabled,
            "mode": schedule.mode,
            "interval_minutes": schedule.interval_minutes,
            "cron_expr": schedule.cron_expr,
            "preset_id": schedule.preset_id,
            "check_ids": schedule.check_ids,
            "db_id": schedule.db_id,
            "next_run_at": schedule.next_run_at,
        }),
    );
    Ok(Json(json!({
        "schedule": schedule,
        "target": target_json(&target),
        "scope_note": SCOPE_NOTE,
    })))
}

// ── 通知配置 ────────────────────────────────────────────────

fn notification_config_json(
    settings: &NotificationSettings,
    secrets: &NotificationSecrets,
) -> serde_json::Value {
    let dispatcher =
        NotificationDispatcher::from_settings(&settings.email, &settings.webhook, secrets);
    json!({
        "settings": settings,
        "secrets": secrets.masked_view(),
        "channels": dispatcher.channel_ids(),
        "scope_note": SCOPE_NOTE,
    })
}

pub(crate) async fn ops_inspection_get_notifications(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let store = state.ops.inspections.read().await;
    Ok(Json(notification_config_json(
        store.notification_settings(),
        store.notification_secrets(),
    )))
}

#[derive(Deserialize)]
pub(crate) struct NotificationConfigPayload {
    #[serde(default)]
    email: Option<EmailNotificationSettings>,
    #[serde(default)]
    webhook: Option<WebhookNotificationSettings>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    backoff_seconds: Option<u64>,
    #[serde(default)]
    max_per_window: Option<usize>,
    #[serde(default)]
    window_seconds: Option<i64>,
    /// 仅在提供时覆盖；空字符串表示清除。
    #[serde(default)]
    smtp_password: Option<String>,
    /// 仅在提供时覆盖；空字符串表示清除。
    #[serde(default)]
    webhook_url: Option<String>,
    /// 提供时整体替换自定义头；空对象表示清除。
    #[serde(default)]
    webhook_headers: Option<std::collections::BTreeMap<String, String>>,
}

pub(crate) async fn ops_inspection_save_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<NotificationConfigPayload>,
) -> Result<Json<serde_json::Value>, AppError> {
    authorize_write(&state, &headers)?;

    let (mut settings, mut secrets) = {
        let store = state.ops.inspections.read().await;
        (
            store.notification_settings().clone(),
            store.notification_secrets().clone(),
        )
    };
    if let Some(email) = payload.email {
        settings.email = email;
    }
    if let Some(webhook) = payload.webhook {
        settings.webhook = webhook;
    }
    if let Some(max_retries) = payload.max_retries {
        settings.max_retries = max_retries.min(10);
    }
    if let Some(backoff_seconds) = payload.backoff_seconds {
        settings.backoff_seconds = backoff_seconds.min(600);
    }
    if let Some(max_per_window) = payload.max_per_window {
        settings.max_per_window = max_per_window.max(1);
    }
    if let Some(window_seconds) = payload.window_seconds {
        settings.window_seconds = window_seconds.max(1);
    }
    if let Some(password) = payload.smtp_password {
        secrets.smtp_password = password;
    }
    if let Some(url) = payload.webhook_url {
        secrets.webhook_url = url;
    }
    if let Some(headers) = payload.webhook_headers {
        let mut cleaned = std::collections::BTreeMap::new();
        for (name, value) in headers {
            if !name.trim().is_empty() && !value.is_empty() {
                cleaned.insert(name.trim().to_string(), value);
            }
        }
        secrets.webhook_headers = cleaned;
    }

    if settings.email.enabled {
        if settings.email.host.trim().is_empty() {
            return Err(AppError::BadRequest("启用邮件通知时必须填写 SMTP 主机".into()));
        }
        if settings.email.to.is_empty() {
            return Err(AppError::BadRequest("启用邮件通知时必须至少填写一个收件人".into()));
        }
        if settings.email.from.trim().is_empty() && settings.email.username.trim().is_empty() {
            return Err(AppError::BadRequest(
                "启用邮件通知时必须填写发件人或用户名".into(),
            ));
        }
    }
    if settings.webhook.enabled && secrets.webhook_url.trim().is_empty() {
        return Err(AppError::BadRequest(
            "启用 Webhook 通知时必须填写 Webhook URL（保存在本地 0600 凭据文件中）".into(),
        ));
    }

    {
        let mut store = state.ops.inspections.write().await;
        store.save_notification_settings(settings.clone())?;
        store.save_notification_secrets(secrets.clone())?;
    }
    audit_event(
        &state,
        "inspection_notifications_saved",
        "ok",
        json!({
            "email_enabled": settings.email.enabled,
            "email_host": settings.email.host,
            "email_to": settings.email.to.len(),
            "webhook_enabled": settings.webhook.enabled,
            "webhook_header_names": settings.webhook.header_names,
            "max_retries": settings.max_retries,
            "backoff_seconds": settings.backoff_seconds,
            "max_per_window": settings.max_per_window,
            "window_seconds": settings.window_seconds,
            "smtp_password_set": !secrets.smtp_password.is_empty(),
            "webhook_url_set": !secrets.webhook_url.is_empty(),
            "scope_note": SCOPE_NOTE,
        }),
    );
    Ok(Json(notification_config_json(&settings, &secrets)))
}

// ── 运行 ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct RunPayload {
    #[serde(default)]
    db_id: Option<String>,
    #[serde(default)]
    preset_id: Option<String>,
    #[serde(default)]
    check_ids: Option<Vec<String>>,
    #[serde(default)]
    thresholds: Option<InspectionThresholds>,
}

pub(crate) async fn ops_inspection_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RunPayload>,
) -> Result<Json<InspectionRunResult>, AppError> {
    authorize_write(&state, &headers)?;
    let schedule = state.ops.inspections.read().await.schedule().clone();
    let db_id = payload
        .db_id
        .clone()
        .or_else(|| (!schedule.db_id.trim().is_empty()).then(|| schedule.db_id.clone()));
    let preset_id = payload
        .preset_id
        .clone()
        .unwrap_or_else(|| {
            if schedule.preset_id.is_empty() {
                "quick_health".into()
            } else {
                schedule.preset_id.clone()
            }
        });
    let explicit = payload.check_ids.clone().unwrap_or_else(|| {
        if schedule.preset_id == preset_id {
            schedule.check_ids.clone()
        } else {
            Vec::new()
        }
    });
    let mut result = execute_inspection(
        &state,
        db_id.as_deref(),
        &preset_id,
        explicit,
        payload.thresholds,
        "manual",
    )
    .await?;
    // 展示与通知前的最后一道防线：历史/驱动错误里可能夹带连接串。
    scrub_run_result(&mut result, &[]);
    Ok(Json(result))
}

pub(crate) async fn execute_inspection(
    state: &AppState,
    db_id: Option<&str>,
    preset_id: &str,
    explicit_check_ids: Vec<String>,
    thresholds: Option<InspectionThresholds>,
    trigger: &str,
) -> Result<InspectionRunResult, AppError> {
    let check_ids = resolve_check_ids(preset_id, &explicit_check_ids).ok_or_else(|| {
        AppError::BadRequest(format!(
            "未知巡检预设或自定义检查项为空: {preset_id}；可选检查项: {}",
            all_check_ids().join(", ")
        ))
    })?;
    let target = resolve_target(state, db_id).await?;
    let ctx = build_ctx(state, target.id.as_deref(), None).await?;
    let (history, active_unused_alert_keys) = {
        let store = state.ops.inspections.read().await;
        (
            store.snapshots_for_fingerprint(&ctx.db_fingerprint, 30),
            store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX),
        )
    };
    let snapshot = run_inspection(
        &ctx,
        &check_ids,
        &thresholds.unwrap_or_default(),
        trigger,
        &history,
        &active_unused_alert_keys,
    )
    .await?;

    let result = {
        let mut store = state.ops.inspections.write().await;
        store.record_snapshot(snapshot)?
    };

    dispatch_notifications(state, &result).await;

    let diff = &result.diff;
    audit_event(
        state,
        "inspection_run",
        "ok",
        json!({
            "trigger": trigger,
            "preset_id": preset_id,
            "check_ids": check_ids,
            "db_id": ctx.db_id,
            "db_name": ctx.db_name,
            "db_fingerprint": ctx.db_fingerprint,
            "snapshot_id": result.snapshot.snapshot_id,
            "status": result.snapshot.status,
            "evidence_count": result.snapshot.summary().evidence_count,
            "diff_added": diff.added.len(),
            "diff_resolved": diff.resolved.len(),
            "diff_changed": diff.changed.len(),
            "alerts_touched": result.alerts.len(),
        }),
    );
    Ok(result)
}

/// 发送本次巡检产生的事件（新建 / 升级 / 恢复），并把每个通道的投递状态写回告警。
/// 未变化的活跃 finding 不会产生事件，因此不会重复推送。
async fn dispatch_notifications(state: &AppState, result: &InspectionRunResult) {
    if result.events.is_empty() {
        return;
    }
    let (settings, secrets) = {
        let store = state.ops.inspections.read().await;
        (
            store.notification_settings().clone(),
            store.notification_secrets().clone(),
        )
    };
    let dispatcher = NotificationDispatcher::from_settings(
        &settings.email,
        &settings.webhook,
        &secrets,
    );
    let policy = RetryPolicy {
        max_retries: settings.max_retries,
        backoff_seconds: settings.backoff_seconds,
    };

    for event in &result.events {
        let mut deliveries: Vec<ChannelDelivery> = Vec::new();
        // 站内通道：告警已写入本地存储，直接记为成功。
        let now = now_ts();
        let mut in_app = ChannelDelivery::pending("in_app");
        in_app.status = DeliveryStatus::Success;
        in_app.attempts = 1;
        in_app.last_attempt_at = Some(now);
        in_app.delivered_at = Some(now);
        deliveries.push(in_app);

        for channel in dispatcher.channels() {
            if channel.id() == "in_app" {
                continue;
            }
            // 全局发送限流：每个事件 × 外部通道消费一个配额。
            let allowed = {
                let mut store = state.ops.inspections.write().await;
                match store.try_acquire_send_slot(
                    now_ts(),
                    settings.window_seconds,
                    settings.max_per_window,
                ) {
                    Ok(allowed) => allowed,
                    Err(err) => {
                        tracing::warn!("检查通知限流状态失败: {}", err);
                        true
                    }
                }
            };
            if !allowed {
                deliveries.push(ChannelDelivery::skipped(
                    channel.id(),
                    format!(
                        "触发每 {} 秒最多 {} 次发送上限，本次未发送",
                        settings.window_seconds, settings.max_per_window
                    ),
                ));
                continue;
            }
            let delivery = deliver_with_retry(channel.as_ref(), event, policy).await;
            if delivery.status == DeliveryStatus::Failed {
                tracing::warn!(
                    channel = channel.id(),
                    attempts = delivery.attempts,
                    error = ?delivery.last_error,
                    "巡检通知发送失败"
                );
            }
            deliveries.push(delivery);
        }

        let mut store = state.ops.inspections.write().await;
        if let Err(err) = store.update_deliveries(&event.alert.alert_id, deliveries) {
            tracing::warn!("记录通知投递状态失败: {}", err);
        }
    }
}

// ── 读取最近结果 / 历史 / 告警 ──────────────────────────────

#[derive(Deserialize)]
pub(crate) struct DbQuery {
    #[serde(default)]
    db_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    include_resolved: Option<bool>,
}

pub(crate) async fn ops_inspection_latest(
    State(state): State<AppState>,
    Query(query): Query<DbQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let target = resolve_target(&state, query.db_id.as_deref()).await?;
    let store = state.ops.inspections.read().await;
    let mut latest: Option<InspectionSnapshot> = store.latest_for_db_id(&target.id_key).cloned();
    if let Some(snapshot) = latest.as_mut() {
        scrub_snapshot(snapshot, &[]);
    }
    let mut previous = latest
        .as_ref()
        .and_then(|snapshot| store.previous(&snapshot.db_fingerprint).cloned());
    if let Some(snapshot) = previous.as_mut() {
        scrub_snapshot(snapshot, &[]);
    }
    let mut diff: Option<SnapshotDiff> = latest
        .as_ref()
        .map(|snapshot| compare_snapshots(previous.as_ref(), snapshot));
    if let Some(diff) = diff.as_mut() {
        scrub_diff(diff, &[]);
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let mut alerts = store.alerts_for_db_id(&target.id_key, limit);
    if !query.include_resolved.unwrap_or(true) {
        alerts.retain(|alert| alert.state == AlertState::Active);
    }
    for alert in &mut alerts {
        scrub_alert(alert, &[]);
    }
    Ok(Json(json!({
        "target": target_json(&target),
        "snapshot": latest,
        "previous_snapshot_id": previous.as_ref().map(|snapshot| snapshot.snapshot_id.clone()),
        "diff": diff,
        "alerts": alerts,
        "unread_count": store.unread_count_for_db_id(&target.id_key),
    })))
}

pub(crate) async fn ops_inspection_history(
    State(state): State<AppState>,
    Query(query): Query<DbQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let target = resolve_target(&state, query.db_id.as_deref()).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let mut snapshots: Vec<SnapshotSummary> = state
        .ops
        .inspections
        .read()
        .await
        .history_for_db_id(&target.id_key, limit);
    for summary in &mut snapshots {
        let (db_name, _) = scrub_text(&summary.db_name, &[]);
        summary.db_name = db_name;
    }
    Ok(Json(json!({
        "target": target_json(&target),
        "snapshots": snapshots,
    })))
}

pub(crate) async fn ops_inspection_alerts(
    State(state): State<AppState>,
    Query(query): Query<DbQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let target = resolve_target(&state, query.db_id.as_deref()).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let store = state.ops.inspections.read().await;
    let mut alerts: Vec<AlertRecord> = store.alerts_for_db_id(&target.id_key, limit);
    if !query.include_resolved.unwrap_or(true) {
        alerts.retain(|alert| alert.state == AlertState::Active);
    }
    for alert in &mut alerts {
        scrub_alert(alert, &[]);
    }
    Ok(Json(json!({
        "target": target_json(&target),
        "alerts": alerts,
        "unread_count": store.unread_count_for_db_id(&target.id_key),
    })))
}

pub(crate) async fn ops_inspection_ack_alert(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(alert_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    authorize_write(&state, &headers)?;
    let mut alert = state
        .ops
        .inspections
        .write()
        .await
        .acknowledge(&alert_id)?;
    scrub_alert(&mut alert, &[]);
    audit_event(
        &state,
        "inspection_alert_ack",
        "ok",
        json!({"alert_id": alert.alert_id, "fingerprint": alert.fingerprint}),
    );
    Ok(Json(json!({ "alert": alert })))
}
