//! orangeDesk 模块 API —— 内核与能力模块之间的稳定契约。
//!
//! 对应设计文档 §2「内核纪律」与 §3 模块规格：
//! - `Module` 标准生命周期契约：Initialize / Suspend / Resume / Finalize
//! - 模块声明资源需求（§4 性能预算）
//! - 事件驱动纪律（P4）：一切低频状态变化走 [`KernelEvent`]，禁止模块自行高频轮询

use std::collections::BTreeSet;

/// 模块唯一标识（稳定字符串，写入配置与日志）
pub type ModuleId = &'static str;

/// 内核生命周期 / 环境事件。
///
/// 由平台事件源（WinEventHook / Shell 广播 / 电源与会话通知）推送，
/// 内核总线分发到每个在线模块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelEvent {
    /// 内核完成全部模块初始化，进入运行态
    Startup,
    /// 请求有序关机（用户退出 / 会话结束）
    ShutdownRequested,
    /// explorer.exe 重启完成（TaskbarCreated 广播）
    ShellRestarted,
    /// 前台窗口变化（process_name 尽力解析，可能为空字符串）
    ForegroundChanged { process_name: String },
    /// 全屏状态变化（全屏独占游戏 / DRM 内容 → true）
    FullscreenChanged { fullscreen: bool },
    /// 显示器拓扑 / 分辨率 / DPI 变化
    DisplayChanged,
    /// 锁屏状态变化（WTS_SESSION_LOCK / UNLOCK）
    LockScreenChanged { locked: bool },
    /// 电源类型变化（电池 ↔ 外接电源）
    PowerSourceChanged { on_battery: bool },
    /// 节能模式变化（电池节能开启 / 电量低）
    PowerSaverChanged { enabled: bool },
    /// 预算管理器：某模块持续超出资源预算（P6 用户可见）
    BudgetExceeded { module: String },
}

/// 模块降级档位（§5 降级矩阵）。序数越大降级越深，性能仪表盘直接展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DegradeLevel {
    /// 全功能
    Full = 0,
    /// 降帧 / 降材质（如壁纸 60 → 30 fps）
    Reduced = 1,
    /// 静态快照（冻结为最后一帧 / 最后一次数据）
    Snapshot = 2,
    /// 完全暂停（渲染线程让出 GPU，显存 Trim）
    Suspended = 3,
}

/// 模块声明的资源预算（§4 性能预算表的运行时镜像，CI 门禁与仪表盘共用）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceBudget {
    /// 空闲态 CPU 占用上限（%）
    pub idle_cpu_percent: f32,
    /// 常驻内存上限（MB）
    pub max_memory_mb: u32,
    /// 是否需要 GPU（用于 GPU 配额与 Trim 调度）
    pub needs_gpu: bool,
}

/// 模块声明：依赖 + 预算 + 简介，注册阶段一次性交给内核。
#[derive(Debug, Clone)]
pub struct ModuleDeclaration {
    pub id: ModuleId,
    /// 依赖的其他模块 id（内核按拓扑序初始化，依赖缺失则级联 offline）
    pub depends_on: BTreeSet<ModuleId>,
    pub budget: ResourceBudget,
    pub description: &'static str,
}

/// 传给 [`Module::initialize`] 的只读配置段
/// （内核已完成加载 / 迁移 / 默认值填充，段可能缺失，模块需自带默认值，P2）
pub type ModuleConfig = serde_json::Value;

/// 模块运行状态快照（性能仪表盘数据源，P6 用户可见）
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleStatus {
    pub level: DegradeLevel,
    /// 人类可读说明，如 "Acrylic 降级为 Clear：系统版本不支持"
    pub detail: String,
}

impl Default for ModuleStatus {
    fn default() -> Self {
        Self {
            level: DegradeLevel::Full,
            detail: String::new(),
        }
    }
}

/// 模块错误。
///
/// - `Crash`：模块自身失联，内核将其隔离（其他模块不受影响，P3）
/// - `Unsupported`：当前系统不满足能力要求，模块标记 offline 并给出原因（能力探测降级）
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("module crashed: {0}")]
    Crash(String),
    #[error("unsupported on this system: {0}")]
    Unsupported(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("{0}")]
    Other(String),
}

/// IModule —— 所有能力模块的标准生命周期契约（§2 内核纪律）。
///
/// 内核在单一分发线程上同步调用这些方法；模块内部的渲染 / 采集线程
/// 由模块自行管理，并必须在 [`Module::suspend`] 中完全让出资源（GPU Trim、
/// 停止数据拉取）。
pub trait Module: Send {
    /// 模块声明（id / 依赖 / 预算），注册阶段调用一次
    fn declaration(&self) -> ModuleDeclaration;
    /// 初始化：读取配置段、创建平台资源。失败则本模块 offline，不拖累其他模块
    fn initialize(&mut self, config: &ModuleConfig) -> Result<(), ModuleError>;
    /// 处理内核广播事件（生命周期 + 环境事件）
    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError>;
    /// 暂停：停止渲染与数据拉取，释放 GPU 资源（全屏 / 锁屏 / 电池节能触发）
    fn suspend(&mut self) -> Result<(), ModuleError>;
    /// 恢复：从 suspend 回到事件发生前的档位
    fn resume(&mut self) -> Result<(), ModuleError>;
    /// 收尾：释放全部资源，进程退出前按初始化逆序调用
    fn finalize(&mut self) -> Result<(), ModuleError>;
    /// 当前状态快照（仪表盘）
    fn status(&self) -> ModuleStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrade_level_orders_by_depth() {
        // 序关系用于仪表盘排序与降级比较：Suspended 是最深档位
        let mut levels = [
            DegradeLevel::Suspended,
            DegradeLevel::Full,
            DegradeLevel::Reduced,
        ];
        levels.sort();
        assert_eq!(
            levels,
            [
                DegradeLevel::Full,
                DegradeLevel::Reduced,
                DegradeLevel::Suspended
            ]
        );
    }

    #[test]
    fn module_status_default_is_full() {
        assert_eq!(ModuleStatus::default().level, DegradeLevel::Full);
    }
}
