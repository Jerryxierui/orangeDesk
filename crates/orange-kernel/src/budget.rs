//! 性能预算管理器（§4 / D6）。
//!
//! 「性能最好」必须可验收：每个模块在注册时声明预算，运行期由采样方
//! （模块自报 / 进程级归因 / CI 基准脚本）持续喂入 [`UsageSample`]，
//! 管理器负责判定持续超标并广播 `BudgetExceeded`，仪表盘直接读取快照。
//!
//! v0.1 范围：登记 / 观察 / 判定框架 + 单元测试。
//! 真实采样接入点（后续版本）：渲染线程 CPU 时间、per-module 内存归因。

use std::collections::BTreeMap;

use orange_module_api::{ModuleDeclaration, ResourceBudget};

/// 一次资源采样
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageSample {
    pub cpu_percent: f32,
    pub memory_mb: f32,
}

/// 预算判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// 预算内
    Within,
    /// 超预算但未持续（观察中；连续超标才升级）
    Watch,
    /// 持续超标 → 内核广播 BudgetExceeded，模块应进入降级序列
    Exceeded,
}

/// 连续超标多少次才判定 Exceeded（过滤瞬时毛刺）
const EXCEED_STREAK: u32 = 3;

struct Observation {
    declared: ResourceBudget,
    consecutive_over: u32,
    last: Option<UsageSample>,
}

/// 性能预算管理器
pub struct BudgetManager {
    observations: BTreeMap<String, Observation>,
}

/// 仪表盘用快照行
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetReportRow {
    pub module: String,
    pub declared: ResourceBudget,
    pub last_sample: Option<UsageSample>,
    pub verdict: BudgetVerdict,
}

impl BudgetManager {
    pub fn new() -> Self {
        Self {
            observations: BTreeMap::new(),
        }
    }

    /// 登记模块声明预算（supervisor 在模块注册后调用）
    pub fn register(&mut self, decl: &ModuleDeclaration) {
        log::info!(
            "budget '{}' <= {}% cpu idle, {}MB mem, gpu={}",
            decl.id,
            decl.budget.idle_cpu_percent,
            decl.budget.max_memory_mb,
            decl.budget.needs_gpu
        );
        self.observations.insert(
            decl.id.to_string(),
            Observation {
                declared: decl.budget,
                consecutive_over: 0,
                last: None,
            },
        );
    }

    /// 喂入一次采样并返回判定。
    ///
    /// 判定规则：cpu 或 memory 任一超出声明值记一次超标；
    /// 连续 `EXCEED_STREAK` 次超标 → `Exceeded`（只在升级沿返回一次）。
    pub fn observe(&mut self, module: &str, sample: UsageSample) -> BudgetVerdict {
        let Some(obs) = self.observations.get_mut(module) else {
            log::warn!("budget observe for unregistered module '{module}'");
            return BudgetVerdict::Within;
        };
        let over = sample.cpu_percent > obs.declared.idle_cpu_percent
            || sample.memory_mb > obs.declared.max_memory_mb as f32;
        obs.last = Some(sample);
        if !over {
            obs.consecutive_over = 0;
            return BudgetVerdict::Within;
        }
        obs.consecutive_over += 1;
        if obs.consecutive_over >= EXCEED_STREAK {
            log::error!(
                "module '{module}' exceeded budget {} ticks: cpu {:.2}% (<= {:.2}%), mem {:.1}MB (<= {}MB)",
                obs.consecutive_over,
                sample.cpu_percent,
                obs.declared.idle_cpu_percent,
                sample.memory_mb,
                obs.declared.max_memory_mb
            );
            BudgetVerdict::Exceeded
        } else {
            BudgetVerdict::Watch
        }
    }

    /// 仪表盘快照
    pub fn snapshot(&self) -> Vec<BudgetReportRow> {
        self.observations
            .iter()
            .map(|(id, obs)| BudgetReportRow {
                module: id.clone(),
                declared: obs.declared,
                last_sample: obs.last,
                verdict: if obs.consecutive_over >= EXCEED_STREAK {
                    BudgetVerdict::Exceeded
                } else if obs.consecutive_over > 0 {
                    BudgetVerdict::Watch
                } else {
                    BudgetVerdict::Within
                },
            })
            .collect()
    }
}

impl Default for BudgetManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn decl(id: &str, cpu: f32, mem: u32) -> ModuleDeclaration {
        ModuleDeclaration {
            id: Box::leak(id.to_string().into_boxed_str()),
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: cpu,
                max_memory_mb: mem,
                needs_gpu: false,
            },
            description: "test",
        }
    }

    #[test]
    fn within_budget_stays_within() {
        let mut mgr = BudgetManager::new();
        mgr.register(&decl("wallpaper", 3.0, 120));
        let verdict = mgr.observe(
            "wallpaper",
            UsageSample {
                cpu_percent: 2.1,
                memory_mb: 90.0,
            },
        );
        assert_eq!(verdict, BudgetVerdict::Within);
    }

    #[test]
    fn requires_consecutive_overs_before_exceeded() {
        let mut mgr = BudgetManager::new();
        mgr.register(&decl("wallpaper", 3.0, 120));
        let hot = UsageSample {
            cpu_percent: 9.5,
            memory_mb: 90.0,
        };

        assert_eq!(mgr.observe("wallpaper", hot), BudgetVerdict::Watch);
        assert_eq!(mgr.observe("wallpaper", hot), BudgetVerdict::Watch);
        // 瞬时回落清零连击
        assert_eq!(
            mgr.observe(
                "wallpaper",
                UsageSample {
                    cpu_percent: 1.0,
                    memory_mb: 50.0
                }
            ),
            BudgetVerdict::Within
        );
        assert_eq!(mgr.observe("wallpaper", hot), BudgetVerdict::Watch);

        // 连续 3 次超标 → Exceeded
        assert_eq!(mgr.observe("wallpaper", hot), BudgetVerdict::Watch);
        assert_eq!(mgr.observe("wallpaper", hot), BudgetVerdict::Exceeded);
    }

    #[test]
    fn memory_alone_can_exceed() {
        let mut mgr = BudgetManager::new();
        mgr.register(&decl("widgets", 0.5, 30));
        let fat = UsageSample {
            cpu_percent: 0.1,
            memory_mb: 512.0,
        };
        for _ in 0..EXCEED_STREAK {
            mgr.observe("widgets", fat);
        }
        let row = mgr
            .snapshot()
            .into_iter()
            .find(|r| r.module == "widgets")
            .unwrap();
        assert_eq!(row.verdict, BudgetVerdict::Exceeded);
    }

    #[test]
    fn unknown_module_is_ignored() {
        let mut mgr = BudgetManager::new();
        assert_eq!(
            mgr.observe(
                "ghost",
                UsageSample {
                    cpu_percent: 99.0,
                    memory_mb: 9999.0
                }
            ),
            BudgetVerdict::Within
        );
    }
}
