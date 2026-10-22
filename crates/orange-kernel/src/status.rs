//! 性能仪表盘快照（§6 v0.2 / §5：各模块实时读数 + 当前降级状态，P6 用户可见）。
//!
//! supervisor 在采样周期（2s）把快照原子写入 `<config_dir>/status.json`：
//! 设置面板卫星进程 / `orange-desk --status` / 外部脚本消费同一份数据。

use std::path::Path;

use serde::{Deserialize, Serialize};

use orange_module_api::DegradeLevel;

use crate::registry::ModuleState;
use crate::sampling::ProcessSample;

pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// 一次完整快照
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub schema_version: u32,
    pub timestamp_ms: u64,
    /// 进程级读数（macOS 开发机等无采样平台为 None）
    pub process: Option<ProcessSample>,
    pub modules: Vec<ModuleStatusRow>,
    /// 预算表（§4/§5：声明预算 + 最近采样 + 判定，含 process.total）
    pub budgets: Vec<crate::budget::BudgetReportRow>,
}

/// 单模块状态行
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleStatusRow {
    pub id: String,
    pub online: bool,
    pub suspended: bool,
    pub crash_count: u32,
    /// 降级档位名（full / reduced / snapshot / suspended）
    pub degrade: String,
    /// 模块自述（材质、降级原因等）
    pub detail: String,
}

/// 降级档位 → 稳定字符串（JSON 消费方按此判定）
pub fn degrade_name(level: DegradeLevel) -> &'static str {
    match level {
        DegradeLevel::Full => "full",
        DegradeLevel::Reduced => "reduced",
        DegradeLevel::Snapshot => "snapshot",
        DegradeLevel::Suspended => "suspended",
    }
}

/// 从注册中心状态 + 进程采样 + 预算快照组装完整快照
pub fn collect<'a>(
    states: impl Iterator<Item = &'a ModuleState>,
    process: Option<ProcessSample>,
    budgets: Vec<crate::budget::BudgetReportRow>,
    timestamp_ms: u64,
) -> StatusSnapshot {
    StatusSnapshot {
        schema_version: STATUS_SCHEMA_VERSION,
        timestamp_ms,
        process,
        budgets,
        modules: states
            .map(|state| ModuleStatusRow {
                id: state.id.clone(),
                online: state.online,
                suspended: state.suspended,
                crash_count: state.crash_count,
                degrade: degrade_name(state.status.level).to_string(),
                detail: state.status.detail.clone(),
            })
            .collect(),
    }
}

/// 原子写快照文件（tmp + rename，与配置持久化同款纪律）
pub fn write_file(snapshot: &StatusSnapshot, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(snapshot)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// 读取快照（`--status` / 卫星进程用）
pub fn read_file(path: &Path) -> std::io::Result<StatusSnapshot> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orange_module_api::ModuleStatus;

    fn state(id: &str, online: bool, detail: &str) -> ModuleState {
        ModuleState {
            id: id.to_string(),
            online,
            suspended: false,
            crash_count: 0,
            status: ModuleStatus {
                level: if online {
                    DegradeLevel::Full
                } else {
                    DegradeLevel::Suspended
                },
                detail: detail.to_string(),
            },
            note: String::new(),
        }
    }

    #[test]
    fn snapshot_serializes_with_stable_fields() {
        let states = [
            state("wallpaper", true, "material: full"),
            state("taskbar", false, "render pipeline failed: x"),
        ];
        let snapshot = collect(
            states.iter(),
            Some(ProcessSample {
                cpu_percent: 0.4,
                working_set_mb: 12.5,
            }),
            vec![crate::budget::BudgetReportRow {
                module: "process.total".to_string(),
                declared: orange_module_api::ResourceBudget {
                    idle_cpu_percent: 0.8,
                    max_memory_mb: 60,
                    needs_gpu: false,
                },
                last_sample: None,
                verdict: crate::budget::BudgetVerdict::Within,
            }],
            1_728_000_000_000,
        );
        assert_eq!(snapshot.schema_version, STATUS_SCHEMA_VERSION);
        assert_eq!(snapshot.modules.len(), 2);
        assert_eq!(snapshot.modules[1].degrade, "suspended");
        assert_eq!(snapshot.budgets.len(), 1);

        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains(r#""degrade":"full""#));
        assert!(json.contains(r#""cpu_percent":0.4"#));
        assert!(json.contains(r#""verdict":"within""#));
        // roundtrip
        let back: StatusSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn file_roundtrip_is_atomic_and_readable() {
        let dir = std::env::temp_dir().join(format!("od-status-{}", std::process::id()));
        let path = dir.join("nested/status.json");
        let snapshot = collect(std::iter::empty(), None, Vec::new(), 42);
        write_file(&snapshot, &path).unwrap();
        let loaded = read_file(&path).unwrap();
        assert_eq!(loaded, snapshot);
        // 原子写不留 tmp 残留
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
