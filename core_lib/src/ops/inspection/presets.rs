//! 巡检预设场景与检查项注册表。

use crate::ops::inspection::types::{
    InspectionPreset, CHECK_CHARSET_COLLATION, CHECK_INACTIVE_ACCOUNT, CHECK_MISSING_PRIMARY_KEY,
    CHECK_ORPHAN_FOREIGN_KEY, CHECK_OVER_PRIVILEGED_ACCOUNT, CHECK_PASSWORDLESS_ACCOUNT,
    CHECK_REDUNDANT_INDEX, CHECK_TABLE_CAPACITY, CHECK_UNUSED_INDEX,
};

pub struct CheckDescriptor {
    pub id: &'static str,
    pub title: &'static str,
    pub rule: &'static str,
}

pub const CHECK_DESCRIPTORS: &[CheckDescriptor] = &[
    CheckDescriptor {
        id: CHECK_REDUNDANT_INDEX,
        title: "冗余索引",
        rule: "同一张表内：①两个 BTREE 索引的列序列完全相同（重复索引）；②索引 B 的完整列序列是索引 A 的前缀，且 B 不是唯一索引（前缀覆盖）。PRIMARY 永远保留；唯一索引不会被更长的非唯一索引判为冗余；FULLTEXT/SPATIAL 不参与比较。",
    },
    CheckDescriptor {
        id: CHECK_UNUSED_INDEX,
        title: "从未被使用的索引",
        rule: "performance_schema.table_io_waits_summary_by_index_usage 中 COUNT_STAR = 0 且索引名不是 PRIMARY。单次快照 COUNT_STAR=0 仅记 info；同一数据库同一索引连续 N 次快照（默认 3 次，由 unused_index_min_snapshots 配置）均为 0 才升级为 warning 并告警。实例 uptime 低于 24 小时或较上一次快照记录的 uptime 回退（视为重启）时判定窗口失效，本次记 info、标注 window_reset 且不计入连续次数；只有 COUNT_STAR>0 才恢复已有告警，窗口重置不恢复。",
    },
    CheckDescriptor {
        id: CHECK_MISSING_PRIMARY_KEY,
        title: "无主键表",
        rule: "当前库内 ENGINE 非空的基础表，没有任何名为 PRIMARY 的索引。",
    },
    CheckDescriptor {
        id: CHECK_TABLE_CAPACITY,
        title: "超大表与碎片",
        rule: "超大表：DATA_LENGTH + INDEX_LENGTH ≥ 2 GiB；碎片：DATA_FREE ≥ 100 MiB 且 DATA_FREE / 总大小 ≥ 20%。TABLE_ROWS 为 InnoDB 估算值，证据中会标注。",
    },
    CheckDescriptor {
        id: CHECK_CHARSET_COLLATION,
        title: "字符集与排序规则不一致",
        rule: "表的 TABLE_COLLATION 与库默认 collation 不同；或字符列的 COLLATION_NAME 与所属表 collation 不同。",
    },
    CheckDescriptor {
        id: CHECK_ORPHAN_FOREIGN_KEY,
        title: "孤儿外键",
        rule: "子表外键列全部非 NULL，且 LEFT JOIN 父表后没有任何匹配行。计数上限 100，达到上限标记 truncated。",
    },
    CheckDescriptor {
        id: CHECK_OVER_PRIVILEGED_ACCOUNT,
        title: "权限过宽账号",
        rule: "除 root@localhost 与 mysql.sys/mysql.session/mysql.infoschema 外，账号持有 SUPER / GRANT OPTION / FILE / PROCESS 任一权限；或 Host 为 % 的账号。存在 SUPER/GRANT OPTION 记 critical，其余记 warning。",
    },
    CheckDescriptor {
        id: CHECK_PASSWORDLESS_ACCOUNT,
        title: "无密码账号",
        rule: "authentication_string（MariaDB 为 Password）为空，plugin 不是 auth_socket/unix_socket，且不属于 mysql.sys/mysql.session/mysql.infoschema。未锁定账号记 critical，已锁定账号记 warning。",
    },
    CheckDescriptor {
        id: CHECK_INACTIVE_ACCOUNT,
        title: "长期未登录账号",
        rule: "实例连续运行 ≥ 7 天，且 performance_schema.accounts 中 TOTAL_CONNECTIONS = 0、CURRENT_CONNECTIONS = 0，同时排除 mysql.sys/mysql.session/mysql.infoschema、匿名账号与已锁定账号。实例运行不足 7 天时明确跳过并说明原因。",
    },
];

pub fn check_descriptor(id: &str) -> Option<&'static CheckDescriptor> {
    CHECK_DESCRIPTORS.iter().find(|item| item.id == id)
}

pub fn all_check_ids() -> Vec<String> {
    CHECK_DESCRIPTORS
        .iter()
        .map(|item| item.id.to_string())
        .collect()
}

pub fn presets() -> Vec<InspectionPreset> {
    vec![
        InspectionPreset {
            id: "quick_health".into(),
            title: "基础健康".into(),
            description: "冗余索引、无主键、容量碎片、字符集、孤儿外键；日常最快的一轮体检。".into(),
            check_ids: vec![
                CHECK_REDUNDANT_INDEX.into(),
                CHECK_MISSING_PRIMARY_KEY.into(),
                CHECK_TABLE_CAPACITY.into(),
                CHECK_CHARSET_COLLATION.into(),
                CHECK_ORPHAN_FOREIGN_KEY.into(),
            ],
        },
        InspectionPreset {
            id: "index_hygiene".into(),
            title: "索引治理".into(),
            description: "冗余索引、未使用索引、无主键；聚焦索引膨胀与写放大。".into(),
            check_ids: vec![
                CHECK_REDUNDANT_INDEX.into(),
                CHECK_UNUSED_INDEX.into(),
                CHECK_MISSING_PRIMARY_KEY.into(),
            ],
        },
        InspectionPreset {
            id: "security_audit".into(),
            title: "账号安全".into(),
            description: "权限过宽、无密码、长期未登录账号；只读检查 mysql.user 与 performance_schema。".into(),
            check_ids: vec![
                CHECK_OVER_PRIVILEGED_ACCOUNT.into(),
                CHECK_PASSWORDLESS_ACCOUNT.into(),
                CHECK_INACTIVE_ACCOUNT.into(),
            ],
        },
        InspectionPreset {
            id: "full".into(),
            title: "完整巡检".into(),
            description: "全部 9 项只读检查。".into(),
            check_ids: all_check_ids(),
        },
    ]
}

pub fn preset_by_id(id: &str) -> Option<InspectionPreset> {
    presets().into_iter().find(|item| item.id == id)
}

/// 解析预设：预设 ID 优先；`custom` 使用显式 check_ids；未知预设返回 None。
pub fn resolve_check_ids(preset_id: &str, explicit: &[String]) -> Option<Vec<String>> {
    if preset_id == "custom" {
        let mut ids = Vec::new();
        for id in explicit {
            if check_descriptor(id).is_none() {
                return None;
            }
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.clone());
            }
        }
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    } else {
        preset_by_id(preset_id).map(|preset| preset.check_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cover_every_check_exactly_once_in_full() {
        let full = preset_by_id("full").expect("full preset");
        assert_eq!(full.check_ids.len(), CHECK_DESCRIPTORS.len());
        for descriptor in CHECK_DESCRIPTORS {
            assert!(full.check_ids.contains(&descriptor.id.to_string()));
        }
    }

    #[test]
    fn resolve_preset_and_custom() {
        let ids = resolve_check_ids("index_hygiene", &[]).expect("preset");
        assert_eq!(
            ids,
            vec![
                CHECK_REDUNDANT_INDEX.to_string(),
                CHECK_UNUSED_INDEX.to_string(),
                CHECK_MISSING_PRIMARY_KEY.to_string()
            ]
        );
        let custom = resolve_check_ids(
            "custom",
            &[CHECK_ORPHAN_FOREIGN_KEY.to_string(), "unknown".into()],
        );
        assert!(custom.is_none());
        let custom = resolve_check_ids(
            "custom",
            &[
                CHECK_ORPHAN_FOREIGN_KEY.to_string(),
                CHECK_ORPHAN_FOREIGN_KEY.to_string(),
            ],
        )
        .expect("dedup");
        assert_eq!(custom.len(), 1);
        assert!(resolve_check_ids("custom", &[]).is_none());
        assert!(resolve_check_ids("nope", &[]).is_none());
    }
}
