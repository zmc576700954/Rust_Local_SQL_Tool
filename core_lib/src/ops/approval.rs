//! 审批令牌：绑定计划哈希 + 动作集合 + 数据库指纹，一次性消费。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::ops::error::OpsError;
use crate::ops::types::{hash_token, now, ApprovalRecord, OpsPlan};

/// 字段迁移阶段的独立审批记录。
///
/// 不复用索引动作的 `ApprovalRecord`：字段回填没有 `ActionSpec`，但同样必须绑定
/// 计划哈希、迁移规格哈希、数据库指纹与阶段，一次性、短时效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldApprovalRecord {
    pub approval_id: String,
    pub plan_id: String,
    pub plan_hash: String,
    /// 计划 + 回填配置 + 源/目标规格的稳定哈希。
    pub migration_hash: String,
    pub db_fingerprint: String,
    /// backfill（当前只开放该阶段；retire 仅出脚本，不签发执行令牌）。
    pub phase: String,
    pub token_hash: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub consumed: bool,
    pub consumed_at: Option<i64>,
    pub consumed_by: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ApprovalFile {
    #[serde(default)]
    approvals: HashMap<String, ApprovalRecord>,
    #[serde(default)]
    field_approvals: HashMap<String, FieldApprovalRecord>,
}

#[derive(Debug)]
pub struct ApprovalStore {
    approvals: HashMap<String, ApprovalRecord>,
    field_approvals: HashMap<String, FieldApprovalRecord>,
    path: Option<PathBuf>,
}

impl ApprovalStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            approvals: HashMap::new(),
            field_approvals: HashMap::new(),
            path,
        };
        store.load();
        store
    }

    fn load(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return;
        };
        if let Ok(file) = serde_json::from_str::<ApprovalFile>(&raw) {
            self.approvals = file.approvals;
            self.field_approvals = file.field_approvals;
        }
    }

    fn save(&self) -> Result<(), OpsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = ApprovalFile {
            approvals: self.approvals.clone(),
            field_approvals: self.field_approvals.clone(),
        };
        let raw = serde_json::to_string_pretty(&file)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 铸造一次性审批令牌。返回 (token, record)。令牌本身只返回给调用方，
    /// 落盘只保存 sha256 哈希。
    pub fn issue(
        &mut self,
        plan: &OpsPlan,
        action_ids: &[String],
        ttl_secs: u64,
        actor: Option<String>,
    ) -> Result<(String, ApprovalRecord), OpsError> {
        if action_ids.is_empty() {
            return Err(OpsError::Approval("必须选择至少一个动作".into()));
        }
        for id in action_ids {
            let action = plan
                .action_by_id(id)
                .ok_or_else(|| OpsError::Approval(format!("动作 {id} 不属于计划")))?;
            if !action.risk_class().is_auto_executable() {
                return Err(OpsError::Approval(format!(
                    "动作 {id} 风险等级 {} 不允许自动执行",
                    action.risk_class().as_str()
                )));
            }
        }
        let token = uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string();
        let record = ApprovalRecord {
            approval_id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan.plan_id.clone(),
            plan_hash: plan.plan_hash.clone(),
            db_fingerprint: plan.db_fingerprint.clone(),
            action_ids: action_ids.to_vec(),
            token_hash: hash_token(&token),
            issued_at: now(),
            expires_at: now() + ttl_secs as i64,
            consumed: false,
            consumed_at: None,
            consumed_by: actor,
        };
        self.approvals
            .insert(record.approval_id.clone(), record.clone());
        // 落盘失败则不发令牌（fail closed）。
        self.save()?;
        Ok((token, record))
    }

    /// 校验并一次性消费令牌。
    pub fn consume(
        &mut self,
        token: &str,
        plan: &OpsPlan,
        requested_action_ids: &[String],
    ) -> Result<ApprovalRecord, OpsError> {
        if token.trim().is_empty() {
            return Err(OpsError::Approval("缺少审批令牌".into()));
        }
        let token_hash = hash_token(token);
        let approval_id = self
            .approvals
            .values()
            .find(|a| a.token_hash == token_hash)
            .map(|a| a.approval_id.clone())
            .ok_or_else(|| OpsError::Approval("审批令牌无效".into()))?;

        let record = self
            .approvals
            .get(&approval_id)
            .cloned()
            .ok_or_else(|| OpsError::Approval("审批记录不存在".into()))?;

        if record.consumed {
            return Err(OpsError::Approval("审批令牌已消费，不能重复使用".into()));
        }
        if now() >= record.expires_at {
            return Err(OpsError::Approval("审批令牌已过期".into()));
        }
        if record.plan_id != plan.plan_id || record.plan_hash != plan.plan_hash {
            return Err(OpsError::Approval(
                "审批令牌与当前计划哈希不匹配（计划可能已变化）".into(),
            ));
        }
        if record.db_fingerprint != plan.db_fingerprint {
            return Err(OpsError::Approval("审批令牌绑定的数据库指纹不匹配".into()));
        }
        for id in requested_action_ids {
            if !record.action_ids.iter().any(|x| x == id) {
                return Err(OpsError::Approval(format!(
                    "动作 {id} 不在审批范围内"
                )));
            }
        }

        let rec = {
            let rec = self
                .approvals
                .get_mut(&approval_id)
                .ok_or_else(|| OpsError::Approval("审批记录不存在".into()))?;
            rec.consumed = true;
            rec.consumed_at = Some(now());
            rec.clone()
        };
        self.save()?;
        Ok(rec)
    }

    pub fn list_recent(&self, limit: usize) -> Vec<ApprovalRecord> {
        let mut list: Vec<_> = self.approvals.values().cloned().collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.issued_at));
        list.into_iter().take(limit).collect()
    }

    /// 为字段回填阶段铸造一次性令牌。令牌绑定计划哈希 + 迁移规格哈希 + 库指纹 + 阶段。
    pub fn issue_field(
        &mut self,
        plan: &OpsPlan,
        migration_hash: &str,
        phase: &str,
        ttl_secs: u64,
        actor: Option<String>,
    ) -> Result<(String, FieldApprovalRecord), OpsError> {
        if phase.trim().is_empty() {
            return Err(OpsError::Approval("缺少字段迁移阶段".into()));
        }
        if migration_hash.trim().is_empty() {
            return Err(OpsError::Approval("缺少迁移规格哈希".into()));
        }
        let token = uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string();
        let record = FieldApprovalRecord {
            approval_id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan.plan_id.clone(),
            plan_hash: plan.plan_hash.clone(),
            migration_hash: migration_hash.to_string(),
            db_fingerprint: plan.db_fingerprint.clone(),
            phase: phase.to_string(),
            token_hash: hash_token(&token),
            issued_at: now(),
            expires_at: now() + ttl_secs as i64,
            consumed: false,
            consumed_at: None,
            consumed_by: actor,
        };
        self.field_approvals
            .insert(record.approval_id.clone(), record.clone());
        // 落盘失败则不发令牌（fail closed）。
        self.save()?;
        Ok((token, record))
    }

    /// 校验并一次性消费字段迁移令牌。
    pub fn consume_field(
        &mut self,
        token: &str,
        plan: &OpsPlan,
        migration_hash: &str,
        phase: &str,
    ) -> Result<FieldApprovalRecord, OpsError> {
        self.consume_field_binding(
            token,
            &plan.plan_id,
            &plan.plan_hash,
            &plan.db_fingerprint,
            migration_hash,
            phase,
        )
    }

    /// 进程重启后续跑没有完整 evidence 时，按任务里已绑定的字段校验令牌。
    pub fn consume_field_binding(
        &mut self,
        token: &str,
        plan_id: &str,
        plan_hash: &str,
        db_fingerprint: &str,
        migration_hash: &str,
        phase: &str,
    ) -> Result<FieldApprovalRecord, OpsError> {
        if token.trim().is_empty() {
            return Err(OpsError::Approval("缺少审批令牌".into()));
        }
        let token_hash = hash_token(token);
        let approval_id = self
            .field_approvals
            .values()
            .find(|a| a.token_hash == token_hash)
            .map(|a| a.approval_id.clone())
            .ok_or_else(|| OpsError::Approval("字段迁移审批令牌无效".into()))?;
        let record = self
            .field_approvals
            .get(&approval_id)
            .cloned()
            .ok_or_else(|| OpsError::Approval("字段迁移审批记录不存在".into()))?;
        if record.consumed {
            return Err(OpsError::Approval("审批令牌已消费，不能重复使用".into()));
        }
        if now() >= record.expires_at {
            return Err(OpsError::Approval("审批令牌已过期".into()));
        }
        if record.plan_id != plan_id || record.plan_hash != plan_hash {
            return Err(OpsError::Approval(
                "审批令牌与当前计划哈希不匹配（计划可能已变化）".into(),
            ));
        }
        if record.db_fingerprint != db_fingerprint {
            return Err(OpsError::Approval("审批令牌绑定的数据库指纹不匹配".into()));
        }
        if record.migration_hash != migration_hash {
            return Err(OpsError::Approval(
                "审批令牌与回填配置/规格哈希不匹配（配置可能已变化）".into(),
            ));
        }
        if record.phase != phase {
            return Err(OpsError::Approval(format!(
                "审批令牌阶段为 {}，不能用于 {phase}",
                record.phase
            )));
        }
        let rec = {
            let rec = self
                .field_approvals
                .get_mut(&approval_id)
                .ok_or_else(|| OpsError::Approval("字段迁移审批记录不存在".into()))?;
            rec.consumed = true;
            rec.consumed_at = Some(now());
            rec.clone()
        };
        self.save()?;
        Ok(rec)
    }

    pub fn list_recent_field(&self, limit: usize) -> Vec<FieldApprovalRecord> {
        let mut list: Vec<_> = self.field_approvals.values().cloned().collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.issued_at));
        list.into_iter().take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::types::{ActionSpec, EvidenceSummary, PlanStatus, RollbackPlan};

    fn plan() -> OpsPlan {
        OpsPlan {
            plan_id: "p1".into(),
            scenario_id: "mysql-index-advisor".into(),
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            evidence_hash: "ev".into(),
            diagnosis: "missing index".into(),
            findings: vec![],
            actions: vec![ActionSpec::CreateIndex {
                table: "orders".into(),
                name: "idx_orders_user".into(),
                columns: vec!["user_id".into()],
                unique: false,
                using: None,
            }],
            verification_sql: vec!["SELECT * FROM orders WHERE user_id = 1".into()],
            risks: vec![],
            validation_checks: vec![],
            rollback: RollbackPlan {
                summary: "drop".into(),
                actions: vec![],
                automatic: true,
            },
            code_advice: vec![],
            rewrite_suggestions: vec![],
            verification_explain_analyze: false,
            confidence: 0.8,
            requires_approval: true,
            created_at: 0,
            plan_hash: "hash".into(),
            status: PlanStatus::PendingApproval,
            evidence_summary: Some(EvidenceSummary::default()),
            execution: None,
        }
    }

    #[test]
    fn token_is_bound_to_plan_hash_and_single_use() {
        let plan = plan();
        let mut store = ApprovalStore::new(None);
        let action_ids = vec!["create_index:idx_orders_user".to_string()];
        let (token, _) = store.issue(&plan, &action_ids, 300, None).unwrap();

        let consumed = store.consume(&token, &plan, &action_ids);
        assert!(consumed.is_ok());

        let second = store.consume(&token, &plan, &action_ids);
        assert!(second.unwrap_err().to_string().contains("已消费"));
    }

    #[test]
    fn token_rejects_tampered_plan_hash_and_action_scope() {
        let plan = plan();
        let mut store = ApprovalStore::new(None);
        let action_ids = vec!["create_index:idx_orders_user".to_string()];
        let (token, _) = store.issue(&plan, &action_ids, 300, None).unwrap();

        let mut tampered = plan.clone();
        tampered.plan_hash = "other".into();
        assert!(store.consume(&token, &tampered, &action_ids).is_err());

        let mut other_action = plan.clone();
        other_action.actions.push(ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_other".into(),
            columns: vec!["status".into()],
            unique: false,
            using: None,
        });
        assert!(store
            .consume(&token, &other_action, &["create_index:idx_orders_other".into()])
            .is_err());
    }

    #[test]
    fn expired_token_is_rejected() {
        let plan = plan();
        let mut store = ApprovalStore::new(None);
        let action_ids = vec!["create_index:idx_orders_user".to_string()];
        let (token, _) = store.issue(&plan, &action_ids, 0, None).unwrap();
        // ttl=0 立即过期
        assert!(store.consume(&token, &plan, &action_ids).is_err());
    }

    #[test]
    fn field_token_binds_migration_hash_phase_and_single_use() {
        let plan = plan();
        let mut store = ApprovalStore::new(None);
        let (token, record) = store
            .issue_field(&plan, "mh-1", "backfill", 300, None)
            .unwrap();
        assert_eq!(record.phase, "backfill");

        // 配置/规格哈希变化必须拒绝。
        assert!(store
            .consume_field(&token, &plan, "mh-2", "backfill")
            .is_err());
        // 阶段不匹配必须拒绝。
        assert!(store
            .consume_field(&token, &plan, "mh-1", "retire")
            .is_err());

        let ok = store
            .consume_field_binding(
                &token,
                &plan.plan_id,
                &plan.plan_hash,
                &plan.db_fingerprint,
                "mh-1",
                "backfill",
            )
            .unwrap();
        assert!(ok.consumed);
        assert!(store
            .consume_field(&token, &plan, "mh-1", "backfill")
            .is_err());
    }

    #[test]
    fn field_token_rejects_tampered_plan_and_db() {
        let plan = plan();
        let mut store = ApprovalStore::new(None);
        let (token, _) = store
            .issue_field(&plan, "mh-1", "backfill", 300, None)
            .unwrap();
        let mut tampered = plan.clone();
        tampered.plan_hash = "other".into();
        assert!(store
            .consume_field(&token, &tampered, "mh-1", "backfill")
            .is_err());

        let mut other_db = plan.clone();
        other_db.db_fingerprint = "other".into();
        assert!(store
            .consume_field(&token, &other_db, "mh-1", "backfill")
            .is_err());
    }
}
