//! 进程内巡检调度器。
//!
//! 调度器跟随 web-server 进程生命周期：进程退出即停止，不做系统级计划任务。
//! 每轮都重新读取持久化的计划，因此用户在运维中心启用/停用或修改间隔后立即生效。

use std::time::Duration;

use core_lib::ops::inspection::scheduler::compute_next_run;
use core_lib::ops::inspection::types::now_ts;

use crate::inspection_handlers::execute_inspection;
use crate::state::AppState;

/// 停用或计划异常时的轮询间隔，避免空转。
const IDLE_CHECK: Duration = Duration::from_secs(30);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        run_loop(state).await;
    });
}

async fn run_loop(state: AppState) {
    loop {
        let schedule = state.ops.inspections.read().await.schedule().clone();
        if !schedule.enabled {
            tokio::time::sleep(IDLE_CHECK).await;
            continue;
        }
        if schedule.db_id.trim().is_empty() {
            record_failure(&state, "巡检计划未指定数据库连接，已暂停执行").await;
            tokio::time::sleep(IDLE_CHECK).await;
            continue;
        }

        let now = now_ts();
        let next = match schedule.next_run_at {
            Some(next) => next,
            None => match compute_next_run(&schedule, now) {
                Ok(next) => {
                    let mut store = state.ops.inspections.write().await;
                    let mut updated = store.schedule().clone();
                    if updated.enabled {
                        updated.next_run_at = Some(next);
                        if let Err(err) = store.save_schedule(updated) {
                            tracing::warn!("保存巡检下一次执行时间失败: {}", err);
                        }
                    }
                    next
                }
                Err(err) => {
                    record_failure(&state, format!("巡检计划无法计算下一次执行时间: {err}"))
                        .await;
                    tokio::time::sleep(IDLE_CHECK).await;
                    continue;
                }
            },
        };

        if next > now {
            let wait = wait_seconds(now, next);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }

        tracing::info!(
            preset = %schedule.preset_id,
            db_id = %schedule.db_id,
            "开始进程内定期巡检"
        );
        let result = execute_inspection(
            &state,
            Some(schedule.db_id.as_str()),
            &schedule.preset_id,
            schedule.check_ids.clone(),
            None,
            "schedule",
        )
        .await;
        if let Err(err) = result {
            tracing::warn!("定期巡检执行失败: {}", err);
            record_failure(&state, err.to_string()).await;
            tokio::time::sleep(IDLE_CHECK).await;
        }
    }
}

async fn record_failure(state: &AppState, message: impl Into<String>) {
    let mut store = state.ops.inspections.write().await;
    if let Err(err) = store.record_failed_run(message, now_ts()) {
        tracing::warn!("记录巡检失败状态失败: {}", err);
    }
}

/// 本轮需要等待的秒数：最多每次睡 60 秒，便于计划变更后及时生效。
fn wait_seconds(now: i64, next: i64) -> u64 {
    (next - now).clamp(1, 60) as u64
}

#[cfg(test)]
mod tests {
    use super::wait_seconds;

    #[test]
    fn wait_is_capped_so_schedule_changes_take_effect() {
        assert_eq!(wait_seconds(100, 101), 1);
        assert_eq!(wait_seconds(100, 110), 10);
        assert_eq!(wait_seconds(100, 3_700), 60);
    }
}
