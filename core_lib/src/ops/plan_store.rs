//! 计划存储。落盘为 JSON，便于审计和重启后继续审批/回滚。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::ops::error::OpsError;
use crate::ops::types::{OpsPlan, PlanStatus, PlanSummary};

#[derive(Debug, Default, Serialize, Deserialize)]
struct PlanFile {
    plans: HashMap<String, OpsPlan>,
}

#[derive(Debug)]
pub struct PlanStore {
    plans: HashMap<String, OpsPlan>,
    path: Option<PathBuf>,
}

impl PlanStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            plans: HashMap::new(),
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
        if let Ok(file) = serde_json::from_str::<PlanFile>(&raw) {
            self.plans = file.plans;
        }
    }

    fn save(&self) -> Result<(), OpsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = PlanFile {
            plans: self.plans.clone(),
        };
        let raw = serde_json::to_string_pretty(&file)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn insert(&mut self, plan: OpsPlan) -> Result<(), OpsError> {
        self.plans.insert(plan.plan_id.clone(), plan);
        self.save()
    }

    pub fn get(&self, plan_id: &str) -> Option<OpsPlan> {
        self.plans.get(plan_id).cloned()
    }

    pub fn update(&mut self, plan: OpsPlan) -> Result<(), OpsError> {
        if !self.plans.contains_key(&plan.plan_id) {
            return Err(OpsError::NotFound(format!("计划 {} 不存在", plan.plan_id)));
        }
        self.plans.insert(plan.plan_id.clone(), plan);
        self.save()
    }

    pub fn list(&self, limit: usize) -> Vec<PlanSummary> {
        let mut list: Vec<_> = self.plans.values().map(OpsPlan::summary).collect();
        list.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        list.into_iter().take(limit).collect()
    }

    pub fn set_status(&mut self, plan_id: &str, status: PlanStatus) -> Result<OpsPlan, OpsError> {
        let mut plan = self
            .get(plan_id)
            .ok_or_else(|| OpsError::NotFound(format!("计划 {plan_id} 不存在")))?;
        plan.status = status;
        self.update(plan.clone())?;
        Ok(plan)
    }
}
