//! 小组件契约（§3 B-1：`IWidget` 契约——Metadata + Update + Suspend/Resume/Finalize）。
//!
//! 纪律落地点：
//! - 每组件**独立更新频率**（时钟 1s、系统监控 2s、天气 10min），
//!   禁止全局统一 Update 定时器——频率声明在 [`WidgetMetadata`]
//! - 遮挡即暂停：被完全遮挡 → 暂停数据拉取（渲染层由 Windows 侧窗口管理）
//! - `update` 只做数据更新；`frame` 返回渲染快照（跨平台数据模型，
//!   Windows 上由 D2D 渲染层消费）

use std::time::Duration;

/// 组件错误
#[derive(Debug, thiserror::Error)]
pub enum WidgetError {
    #[error("widget crashed: {0}")]
    Crash(String),
    #[error("{0}")]
    Other(String),
}

/// 遮挡策略（§3 B-1）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcclusionPolicy {
    /// 完全遮挡 → 暂停数据更新与渲染（默认；1 秒内生效）
    Pause,
    /// 遮挡仍保持数据更新（如录音类组件）
    KeepUpdating,
}

/// 组件元数据：更新频率 / 遮挡策略 / 展示名
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetMetadata {
    pub id: &'static str,
    pub display_name: &'static str,
    /// 数据更新间隔（渲染类组件最低 16ms，数据类可到小时级）
    pub update_interval: Duration,
    pub occlusion_policy: OcclusionPolicy,
}

/// 渲染快照（跨平台数据模型；Windows 渲染层消费）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WidgetFrame {
    /// 文本行（v0.3 骨架；D2D 渲染层上线后扩展为绘制指令）
    pub lines: Vec<String>,
}

/// IWidget —— 第一方组件契约（第三方声明式组件在 v1.1 由内核渲染器解释执行）
pub trait IWidget: Send {
    fn metadata(&self) -> WidgetMetadata;
    /// 数据更新（由 EDF 调度器按声明的频率唤醒）
    fn update(&mut self, now_ms: u64) -> Result<(), WidgetError>;
    /// 暂停（遮挡 / 锁屏 / 电池节能）：停止数据拉取，保留最后值
    fn suspend(&mut self);
    /// 恢复（调用方保证随后立即触发一次 update 刷新数据）
    fn resume(&mut self);
    fn finalize(&mut self);
    /// 当前渲染快照
    fn frame(&self) -> WidgetFrame;
}

/// 遮挡 / 全局暂停下的更新决策（纯逻辑，可测）：
/// 全局暂停（锁屏 / 电池节能）一律停；遮挡按组件策略。
pub fn should_update(policy: OcclusionPolicy, occluded: bool, globally_suspended: bool) -> bool {
    if globally_suspended {
        return false;
    }
    if occluded {
        return policy == OcclusionPolicy::KeepUpdating;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occlusion_policy_decides_updates() {
        // 可见：始终更新
        assert!(should_update(OcclusionPolicy::Pause, false, false));
        assert!(should_update(OcclusionPolicy::KeepUpdating, false, false));
        // 遮挡：按策略
        assert!(!should_update(OcclusionPolicy::Pause, true, false));
        assert!(should_update(OcclusionPolicy::KeepUpdating, true, false));
        // 锁屏 / 电池节能（全局暂停）：一律停（数据也停，性能底线）
        assert!(!should_update(OcclusionPolicy::KeepUpdating, false, true));
        assert!(!should_update(OcclusionPolicy::Pause, true, true));
    }
}
