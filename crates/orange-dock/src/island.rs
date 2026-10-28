//! 灵动岛（Dynamic Island 式活动胶囊，§13 悬浮 UI 家族）。
//!
//! 顶部胶囊形顶层窗口（真机 DComp 渲染），按“活动”变形：
//! - 收缩态：显示当前最高优先级活动的单行摘要
//! - 展开态：活动卡片（媒体进度 / 频谱 / 倒计时），超时自动收回
//!
//! 活动源：媒体（`MediaSource`，真机 SMTC）、频谱（`AudioSpectrum` 事件）、
//! 定时器 / 专注（API 就绪，入口随渲染层与托盘接入）。
//! 状态机与快照为纯逻辑（跨平台全测试）；渲染窗口在真机阶段接入。

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use orange_widgets::media::{MediaSource, PlaybackStatus};

/// 岛停靠位置（渲染层消费）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IslandPosition {
    TopCenter,
    TopLeft,
    TopRight,
}

/// 活动种类（去重键）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    Media,
    Spectrum,
    Timer,
    Focus,
    Dnd,
}

/// 岛活动
#[derive(Debug, Clone, PartialEq)]
pub enum IslandActivity {
    /// 媒体播放（收缩：♪ 标题 — 艺术家；展开：进度行）
    Media {
        title: String,
        artist: String,
        status: PlaybackStatus,
        position_ms: u64,
        duration_ms: u64,
    },
    /// 音频频谱（收缩：紧凑条形；展开：宽条形）
    Spectrum { bands: Vec<f32> },
    /// 倒计时 / 定时器（最高优先级；入口随托盘 / 快捷指令接入）
    Timer { label: String, remaining_ms: u64 },
    /// 专注模式（场景联动，入口随 L3 场景切换接入）
    Focus { scene: String },
    /// 免打扰状态（持续状态，最低展示优先级）
    Dnd { enabled: bool },
}

impl IslandActivity {
    pub fn kind(&self) -> ActivityKind {
        match self {
            IslandActivity::Media { .. } => ActivityKind::Media,
            IslandActivity::Spectrum { .. } => ActivityKind::Spectrum,
            IslandActivity::Timer { .. } => ActivityKind::Timer,
            IslandActivity::Focus { .. } => ActivityKind::Focus,
            IslandActivity::Dnd { .. } => ActivityKind::Dnd,
        }
    }

    /// 展示优先级（高者先）：倒计时 > 媒体 > 专注 > DND > 频谱
    pub fn priority(&self) -> u8 {
        match self {
            IslandActivity::Timer { .. } => 100,
            IslandActivity::Media { status, .. } if *status == PlaybackStatus::Playing => 90,
            IslandActivity::Media { .. } => 70,
            IslandActivity::Focus { .. } => 60,
            IslandActivity::Dnd { enabled: true } => 50,
            IslandActivity::Dnd { enabled: false } => 5, // 关闭态几乎不展示
            IslandActivity::Spectrum { .. } => 40,
        }
    }

    /// 收缩态单行摘要
    pub fn summary(&self) -> String {
        match self {
            IslandActivity::Media {
                title,
                artist,
                status,
                ..
            } => {
                let icon = match status {
                    PlaybackStatus::Playing => "♪",
                    PlaybackStatus::Paused => "⏸",
                    _ => "⏹",
                };
                format!("{icon} {title} — {artist}")
            }
            IslandActivity::Spectrum { bands } => compact_bars(bands, 8),
            IslandActivity::Timer {
                label,
                remaining_ms,
            } => {
                format!("⏱ {label} {}", format_duration(*remaining_ms))
            }
            IslandActivity::Focus { scene } => format!("◐ focus · {scene}"),
            IslandActivity::Dnd { enabled } if *enabled => "⛔ Focus".to_string(),
            IslandActivity::Dnd { .. } => "Focus off".to_string(),
        }
    }

    /// 展开态第二行（媒体进度 / 宽频谱 / 倒计时说明）
    pub fn detail(&self) -> String {
        match self {
            IslandActivity::Media {
                position_ms,
                duration_ms,
                ..
            } => format!(
                "{} / {}",
                format_duration(*position_ms),
                format_duration(*duration_ms)
            ),
            IslandActivity::Spectrum { bands } => compact_bars(bands, 16),
            IslandActivity::Timer { remaining_ms, .. } => {
                format!("{} remaining", format_duration(*remaining_ms))
            }
            IslandActivity::Focus { scene } => format!("scene '{scene}' active"),
            IslandActivity::Dnd { enabled } => {
                if *enabled {
                    "notifications muted".to_string()
                } else {
                    "notifications on".to_string()
                }
            }
        }
    }
}

/// 频段 → 块字符条形（与 widgets 的 bars_for 同款映射，独立实现避免跨 crate 依赖扩散）
fn compact_bars(bands: &[f32], slots: usize) -> String {
    const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    if bands.is_empty() || slots == 0 {
        return String::new();
    }
    // 均匀重采样到目标槽位
    (0..slots)
        .map(|slot| {
            let start = slot * bands.len() / slots;
            let end = ((slot + 1) * bands.len() / slots)
                .max(start + 1)
                .min(bands.len());
            let value = bands[start..end].iter().cloned().fold(0.0f32, f32::max);
            let level = (value.clamp(0.0, 1.0) * (LEVELS.len() - 1) as f32).round() as usize;
            LEVELS[level]
        })
        .collect()
}

/// 毫秒 → m:ss / h:mm:ss
pub fn format_duration(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// 活动启停配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IslandActivities {
    pub media: bool,
    pub spectrum: bool,
    pub timer: bool,
    pub focus: bool,
    pub dnd: bool,
}

impl Default for IslandActivities {
    fn default() -> Self {
        Self {
            media: true,
            spectrum: true,
            timer: true,
            focus: true,
            dnd: true,
        }
    }
}

/// 灵动岛配置段（modules.island / 皮肤 island 段）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IslandSettings {
    pub enabled: bool,
    pub position: IslandPosition,
    /// 展开后无交互自动收缩（ms；0 = 不自动）
    pub auto_collapse_ms: u32,
    pub activities: IslandActivities,
}

impl Default for IslandSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            position: IslandPosition::TopCenter,
            auto_collapse_ms: 6000,
            activities: IslandActivities::default(),
        }
    }
}

/// 渲染快照（数据层文本形态；真机 D2D 替换）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IslandFrame {
    /// 无活动 = 隐藏（渲染层不显示窗口）
    pub visible: bool,
    pub expanded: bool,
    pub primary: String,
    pub secondary: String,
}

/// 岛状态机（纯逻辑）：活动池 + 展开/收缩 + 超时收回
#[derive(Debug, Clone, PartialEq)]
pub struct IslandEngine {
    activities: Vec<IslandActivity>,
    expanded: bool,
    expand_deadline_ms: Option<u64>,
    auto_collapse_ms: u32,
}

impl IslandEngine {
    pub fn new(auto_collapse_ms: u32) -> Self {
        Self {
            activities: Vec::new(),
            expanded: false,
            expand_deadline_ms: None,
            auto_collapse_ms,
        }
    }

    /// 活动 upsert（同 kind 替换；更新不改变展开态）
    pub fn upsert(&mut self, activity: IslandActivity) {
        match self
            .activities
            .iter()
            .position(|a| a.kind() == activity.kind())
        {
            Some(index) => self.activities[index] = activity,
            None => self.activities.push(activity),
        }
    }

    /// 移除活动；若移除的是展开中的活动则自动收缩
    pub fn remove(&mut self, kind: ActivityKind) {
        self.activities.retain(|a| a.kind() != kind);
        if self.current().map(|a| a.kind()) != Some(kind) {
            return;
        }
        // 当前展示活动被移除：收回（若还有其他活动则显示新的）
        self.expanded = false;
        self.expand_deadline_ms = None;
    }

    /// 活动池大小（调试 / 测试 / 渲染层）
    pub fn activity_count(&self) -> usize {
        self.activities.len()
    }

    /// 当前应展示的活动（最高优先级）
    pub fn current(&self) -> Option<&IslandActivity> {
        self.activities.iter().max_by_key(|a| a.priority())
    }

    pub fn expand(&mut self, now_ms: u64) {
        self.expanded = true;
        self.expand_deadline_ms = if self.auto_collapse_ms > 0 {
            Some(now_ms + self.auto_collapse_ms as u64)
        } else {
            None
        };
    }

    pub fn collapse(&mut self) {
        self.expanded = false;
        self.expand_deadline_ms = None;
    }

    /// 热配置：调整自动收回时长（不清空活动池——换配置不该打断进行中的活动）
    pub fn set_auto_collapse(&mut self, auto_collapse_ms: u32) {
        self.auto_collapse_ms = auto_collapse_ms;
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// 超时收回检查（SecondTick 驱动）；返回状态是否变化
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if let Some(deadline) = self.expand_deadline_ms {
            if now_ms >= deadline {
                self.collapse();
                return true;
            }
        }
        false
    }

    /// 渲染快照
    pub fn frame(&self) -> IslandFrame {
        let Some(activity) = self.current() else {
            return IslandFrame::default();
        };
        IslandFrame {
            visible: true,
            expanded: self.expanded,
            primary: activity.summary(),
            secondary: if self.expanded {
                activity.detail()
            } else {
                String::new()
            },
        }
    }
}

/// 灵动岛模块（数据层）
pub struct IslandModule {
    settings: IslandSettings,
    engine: IslandEngine,
    /// 媒体源（真机 SMTC；数据层 / 测试用静态源）
    media: Box<dyn MediaSource>,
}

impl IslandModule {
    pub fn new(media: Box<dyn MediaSource>) -> Self {
        Self {
            settings: IslandSettings::default(),
            engine: IslandEngine::new(IslandSettings::default().auto_collapse_ms),
            media,
        }
    }

    /// 注入活动（定时器 / 专注等入口的公共 API；托盘 / 快捷指令接入）
    pub fn inject(&mut self, activity: IslandActivity) {
        if self.activity_enabled(activity.kind()) {
            self.engine.upsert(activity);
        }
    }

    /// 移除活动
    pub fn dismiss(&mut self, kind: ActivityKind) {
        self.engine.remove(kind);
    }

    fn activity_enabled(&self, kind: ActivityKind) -> bool {
        let flags = &self.settings.activities;
        match kind {
            ActivityKind::Media => flags.media,
            ActivityKind::Spectrum => flags.spectrum,
            ActivityKind::Timer => flags.timer,
            ActivityKind::Focus => flags.focus,
            ActivityKind::Dnd => flags.dnd,
        }
    }
}

impl Module for IslandModule {
    fn declaration(&self) -> ModuleDeclaration {
        ModuleDeclaration {
            id: "island",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 0.3,
                max_memory_mb: 10,
                needs_gpu: true,
            },
            description: "dynamic island (activity capsule: media/spectrum/timer/focus, expand+auto-collapse)",
        }
    }

    fn initialize(&mut self, config: &ModuleConfig) -> Result<(), ModuleError> {
        self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
        if !self.settings.enabled {
            return Err(ModuleError::Config("island disabled by config".to_string()));
        }
        self.engine = IslandEngine::new(self.settings.auto_collapse_ms);
        // TODO(真机): DComp 顶层圆角胶囊窗口 + 悬停/点击展开 + 形变动画
        log::info!("island: ready ({:?})", self.settings.position);
        Ok(())
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            // 频谱活动（音频模块 ~30Hz 广播；岛只保留最新一帧）
            KernelEvent::AudioSpectrum { bands } => {
                if self.settings.activities.spectrum {
                    self.engine.upsert(IslandActivity::Spectrum {
                        bands: bands.clone(),
                    });
                }
            }
            // 秒级心跳：媒体轮询 + 展开超时检查
            KernelEvent::SecondTick => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if self.settings.activities.media {
                    match self.media.current() {
                        Some(track) => self.engine.upsert(IslandActivity::Media {
                            title: track.title,
                            artist: track.artist,
                            status: track.status,
                            position_ms: track.position_ms,
                            duration_ms: track.duration_ms,
                        }),
                        None => self.engine.remove(ActivityKind::Media),
                    }
                }
                self.engine.poll(now_ms);
            }
            KernelEvent::ModuleConfigChanged { config, .. } => {
                self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
                self.engine
                    .set_auto_collapse(self.settings.auto_collapse_ms);
                // 被新配置禁用的活动种类立即退出展示
                for kind in [
                    ActivityKind::Media,
                    ActivityKind::Spectrum,
                    ActivityKind::Timer,
                    ActivityKind::Focus,
                    ActivityKind::Dnd,
                ] {
                    if !self.activity_enabled(kind) {
                        self.engine.remove(kind);
                    }
                }
                log::info!("island: config reloaded");
            }
            // TODO(真机): 锁屏 / 节能 → 渲染层隐藏岛
            _ => {}
        }
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), ModuleError> {
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ModuleError> {
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), ModuleError> {
        // TODO(真机): 销毁渲染窗口
        Ok(())
    }

    fn status(&self) -> ModuleStatus {
        let frame = self.engine.frame();
        ModuleStatus {
            level: DegradeLevel::Full,
            detail: if frame.visible {
                format!(
                    "{}{}",
                    if frame.expanded { "expanded: " } else { "" },
                    frame.primary
                )
            } else {
                "hidden (no activity)".to_string()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orange_widgets::media::{MediaTrack, StaticMediaSource};

    fn media_activity(title: &str, playing: bool) -> IslandActivity {
        IslandActivity::Media {
            title: title.to_string(),
            artist: "Artist".to_string(),
            status: if playing {
                PlaybackStatus::Playing
            } else {
                PlaybackStatus::Paused
            },
            position_ms: 61_000,
            duration_ms: 200_000,
        }
    }

    #[test]
    fn empty_island_is_hidden() {
        let engine = IslandEngine::new(6000);
        let frame = engine.frame();
        assert!(!frame.visible);
        assert_eq!(frame, IslandFrame::default());
    }

    #[test]
    fn media_summary_and_detail_format() {
        let activity = media_activity("Nocturne", true);
        assert_eq!(activity.summary(), "♪ Nocturne — Artist");
        assert_eq!(activity.detail(), "1:01 / 3:20");
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn upsert_replaces_same_kind_and_updates() {
        let mut engine = IslandEngine::new(6000);
        engine.upsert(media_activity("A", true));
        engine.upsert(media_activity("B", true));
        assert_eq!(engine.activity_count(), 1, "same kind replaces");
        assert!(engine.current().unwrap().summary().contains("B"));
    }

    #[test]
    fn timer_outranks_media_and_spectrum() {
        let mut engine = IslandEngine::new(6000);
        engine.upsert(media_activity("Song", true));
        engine.upsert(IslandActivity::Spectrum {
            bands: vec![0.5; 16],
        });
        assert!(
            engine.current().unwrap().summary().contains("Song"),
            "playing media beats spectrum"
        );
        engine.upsert(IslandActivity::Timer {
            label: "Tea".into(),
            remaining_ms: 90_000,
        });
        assert!(
            engine.current().unwrap().summary().contains("Tea"),
            "timer beats all"
        );
        engine.remove(ActivityKind::Timer);
        assert!(
            engine.current().unwrap().summary().contains("Song"),
            "fallback after removal"
        );
    }

    #[test]
    fn expand_shows_detail_and_auto_collapses() {
        let mut engine = IslandEngine::new(6000);
        engine.upsert(media_activity("Song", true));
        engine.expand(1_000);
        let frame = engine.frame();
        assert!(frame.expanded);
        assert_eq!(frame.secondary, "1:01 / 3:20");

        assert!(!engine.poll(6_500), "before deadline unchanged");
        assert!(engine.poll(7_001), "deadline crosses -> collapsed");
        assert!(!engine.frame().expanded);
    }

    #[test]
    fn spectrum_resamples_to_compact_bars() {
        let bands: Vec<f32> = (0..16).map(|i| i as f32 / 16.0).collect();
        let activity = IslandActivity::Spectrum { bands };
        // 16 带递增 → 8 槽取每 2 带最大值 → 上升条形，末槽满格
        assert!(activity.summary().ends_with('█'));
        assert!(activity.summary().starts_with('▁'));
        assert_eq!(compact_bars(&[], 8), "");
    }

    #[test]
    fn settings_parse_and_default_enabled() {
        let json = serde_json::json!({
            "position": "topright",
            "auto_collapse_ms": 3000,
            "activities": { "spectrum": false }
        });
        let settings: IslandSettings = serde_json::from_value(json).unwrap();
        assert_eq!(settings.position, IslandPosition::TopRight);
        assert!(!settings.activities.spectrum);
        assert!(settings.activities.media);
        let default = IslandSettings::default();
        assert!(default.enabled);
        assert_eq!(default.auto_collapse_ms, 6000);
    }

    #[test]
    fn module_tracks_media_and_spectrum_events() {
        let media = StaticMediaSource::with_track(MediaTrack {
            title: "Lofi".into(),
            artist: "Radio".into(),
            album: String::new(),
            position_ms: 30_000,
            duration_ms: 120_000,
            status: PlaybackStatus::Playing,
        });
        let mut module = IslandModule::new(Box::new(media));
        module.initialize(&serde_json::Value::Null).unwrap();

        // 频谱事件 → Spectrum 活动（频谱条形可见）
        module
            .on_event(&KernelEvent::AudioSpectrum {
                bands: vec![0.8; 16],
            })
            .unwrap();
        assert!(module.status().detail.contains('▇'), "spectrum bars shown");

        // SecondTick → 媒体轮询 upsert，播放中媒体(90)高于频谱(40)
        module.on_event(&KernelEvent::SecondTick).unwrap();
        assert!(module.status().detail.contains("Lofi"));
        assert!(module.engine.frame().primary.contains("Lofi"));

        // 配置禁用媒体 → 移除后回到频谱
        let cfg = serde_json::json!({ "activities": { "media": false } });
        module
            .on_event(&KernelEvent::ModuleConfigChanged {
                module: "island".into(),
                config: cfg,
            })
            .unwrap();
        module.on_event(&KernelEvent::SecondTick).unwrap();
        assert!(
            module.engine.frame().primary.contains('▇'),
            "back to spectrum after media disabled"
        );
    }

    #[test]
    fn inject_respects_activity_flags() {
        let mut module = IslandModule::new(Box::new(StaticMediaSource::empty()));
        module
            .initialize(&serde_json::json!({"activities": {"timer": false}}))
            .unwrap();
        module.inject(IslandActivity::Timer {
            label: "x".into(),
            remaining_ms: 60_000,
        });
        assert!(
            !module.engine.frame().visible,
            "disabled activity never enters the pool"
        );
    }

    #[test]
    fn disabled_island_reports_config_error() {
        let mut module = IslandModule::new(Box::new(StaticMediaSource::empty()));
        assert!(matches!(
            module.initialize(&serde_json::json!({"enabled": false})),
            Err(ModuleError::Config(_))
        ));
    }
}
