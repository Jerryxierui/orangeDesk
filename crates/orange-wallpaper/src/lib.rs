//! orangeDesk 壁纸模块（§3 模块 A）。
//!
//! 技术路线（D1/D3，吸取 FeatherWall / Wallpaper Engine 经验）：
//! - 拓扑适配层一等公民：Classic（WorkerW）/ Raised（Win11 24H2+ Progman 子窗口）
//!   双路径 + 回退，运行时探测
//! - 渲染管线按 DirectComposition flip-model 提交模型设计；GDI/blt 永不作为主路径
//! - 暂停而非降帧：全屏独占 / DRM / 锁屏 / 电池节能 → 完全暂停渲染 + 显存 Trim
//!
//! v0.1 范围：拓扑探测 + 挂载 + 纯色 / 图片（WIC + D2D DrawBitmap）路径；
//! 视频硬解（video processor blit）TODO 标注，v0.1 内补齐。

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

// 纯几何逻辑：调用方（renderer）仅 Windows；非 Windows 保留供单元测试
#[cfg_attr(not(windows), allow(dead_code))]
mod geometry;
pub mod monitor;
#[cfg(windows)]
mod mount;
// 帧率档位调度：调用方（renderer）仅 Windows；非 Windows 保留供单元测试
#[cfg_attr(not(windows), allow(dead_code))]
mod pacing;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod topology;
#[cfg(windows)]
mod video;

/// 帧率档位（§3 A-1：30 / 60 / 120 / 无限，双缓冲 + VSync；
/// T15 为 §5 降级矩阵的压力专用档）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FpsTier {
    T15,
    T30,
    T60,
    T120,
    Unlimited,
}

impl FpsTier {
    /// 目标帧间隔（ms）；Unlimited = 0（仅 VSync 对齐）
    pub fn frame_interval_ms(self) -> u64 {
        match self {
            FpsTier::T15 => 66,
            FpsTier::T30 => 33,
            FpsTier::T60 => 16,
            FpsTier::T120 => 8,
            FpsTier::Unlimited => 0,
        }
    }

    /// §5 降级矩阵的下一档（60/120/无限 → 30 → 15 → None = 静态快照即暂停）
    fn degrade_step(self) -> Option<FpsTier> {
        match self {
            FpsTier::Unlimited | FpsTier::T120 | FpsTier::T60 => Some(FpsTier::T30),
            FpsTier::T30 => Some(FpsTier::T15),
            FpsTier::T15 => None,
        }
    }
}

/// 壁纸内容模式。v0.1 实现纯色；图片 / 视频路径已预留配置位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WallpaperMode {
    Solid,
    Image,
    Video,
}

/// 壁纸模块配置段（modules.wallpaper），缺失时全部取默认值（P2）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WallpaperSettings {
    pub mode: WallpaperMode,
    /// 纯色壁纸颜色（#RRGGBB），默认品牌橙
    pub solid_color: String,
    pub image_path: Option<String>,
    pub video_path: Option<String>,
    pub fps_tier: FpsTier,
    /// 播放列表（§13 F1，Wallpaper Engine 同款能力）：存在时覆盖
    /// 单源 mode，按间隔在条目间轮换
    pub playlist: Option<WallpaperPlaylist>,
}

/// 播放列表配置（modules.wallpaper.playlist）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WallpaperPlaylist {
    /// 壁纸文件路径（图片 / 视频按扩展名识别）
    pub entries: Vec<String>,
    /// 切换间隔（分钟），最低 1
    pub interval_min: u32,
    /// 轮换顺序
    pub order: PlaylistOrder,
}

impl Default for WallpaperPlaylist {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            interval_min: 30,
            order: PlaylistOrder::Sequential,
        }
    }
}

/// 轮换顺序
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaylistOrder {
    /// 顺序循环
    Sequential,
    /// 随机（种子可注入，测试确定性）
    Random,
}

/// 播放列表调度状态（纯逻辑，跨平台可测）
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistState {
    entries: Vec<String>,
    order: PlaylistOrder,
    current: usize,
    /// LCG 状态（Random 模式）；种子可注入保证测试确定性
    rng: u64,
}

impl PlaylistState {
    pub fn new(playlist: &WallpaperPlaylist, rng_seed: u64) -> Self {
        Self {
            entries: playlist.entries.clone(),
            order: playlist.order,
            current: 0,
            rng: rng_seed.max(1),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn current_path(&self) -> Option<&str> {
        self.entries.get(self.current).map(|s| s.as_str())
    }

    /// 切到下一个条目（循环）；返回新条目路径，空列表返回 None
    pub fn advance(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        match self.order {
            PlaylistOrder::Sequential => {
                self.current = (self.current + 1) % self.entries.len();
            }
            PlaylistOrder::Random => {
                // LCG 步进（数值哈希质量足够壁纸轮换场景）
                self.rng = self
                    .rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.current = ((self.rng >> 33) as usize) % self.entries.len();
            }
        }
        self.current_path()
    }
}

impl Default for WallpaperSettings {
    fn default() -> Self {
        Self {
            mode: WallpaperMode::Solid,
            solid_color: "#F76B1C".to_string(),
            image_path: None,
            video_path: None,
            fps_tier: FpsTier::T60,
            playlist: None,
        }
    }
}

/// 解析 `#RRGGBB` 为线性 RGBA（0.0–1.0，D3D Clear 使用 sRGB 数值直接输出）
pub fn parse_hex_color(input: &str) -> Option<[f32; 4]> {
    let hex = input.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    Some([
        byte(0..2)? as f32 / 255.0,
        byte(2..4)? as f32 / 255.0,
        byte(4..6)? as f32 / 255.0,
        1.0,
    ])
}

/// 按扩展名把播放列表条目映射为内容源（§13 F1；未知扩展名 → 纯色降级）
#[cfg(windows)]
fn content_source_for_path(path: &str, fallback_color: [f32; 4]) -> renderer::ContentSource {
    let lower = path.to_ascii_lowercase();
    let is_video = [".mp4", ".mkv", ".webm", ".mov", ".avi", ".m4v"]
        .iter()
        .any(|ext| lower.ends_with(ext));
    if is_video {
        renderer::ContentSource::Video {
            path: path.to_string(),
            fallback_color,
        }
    } else if lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".bmp")
        || lower.ends_with(".tif")
        || lower.ends_with(".tiff")
        || lower.ends_with(".gif")
    {
        renderer::ContentSource::Image {
            path: path.to_string(),
            fallback_color,
        }
    } else {
        log::warn!("wallpaper: unrecognized extension, solid fallback ({path})");
        renderer::ContentSource::Solid {
            color: fallback_color,
        }
    }
}

/// 模块运行态（status() 的数据源）
#[derive(Debug, Clone, PartialEq)]
enum WallpaperState {
    /// 初始化前
    Created,
    /// 正常运行（附降级说明，如 "solid fallback: video pipeline not yet implemented"）
    Running { detail: String },
    /// 完全暂停（全屏 / 锁屏 / 节能），渲染线程让出 GPU + Trim
    Suspended { reason: String },
    /// 不可用（能力探测失败 / 非 Windows），附原因
    Offline { reason: String },
}

/// 壁纸模块
pub struct WallpaperModule {
    settings: WallpaperSettings,
    state: WallpaperState,
    /// 当前生效帧率档位（settings 的运行副本；§5 降级矩阵只降不升，
    /// 环境事件恢复（fullscreen false 等）不会回升档位，重启进程恢复）
    fps_tier: FpsTier,
    /// 播放列表运行态（§13 F1；None = 单源模式）
    playlist: Option<PlaylistState>,
    /// 距下次播放列表切换剩余分钟（MinuteTick 递减）
    minutes_to_switch: u32,
    /// 暂停期间到期的切换（恢复后补执行，避免锁屏时重启引擎复活渲染）
    pending_playlist_switch: bool,
    #[cfg(windows)]
    engine: Option<renderer::Renderer>,
}

impl WallpaperModule {
    pub fn new() -> Self {
        Self {
            settings: WallpaperSettings::default(),
            state: WallpaperState::Created,
            fps_tier: WallpaperSettings::default().fps_tier,
            playlist: None,
            minutes_to_switch: 0,
            pending_playlist_switch: false,
            #[cfg(windows)]
            engine: None,
        }
    }

    fn set_suspend_reason(reason: &str) -> String {
        reason.to_string()
    }

    /// §5 降级矩阵首行：预算压力下调帧率 60/120/无限 → 30 → 15 → 静态快照（暂停）。
    /// 触发源：`BudgetExceeded`（当前为 process.total 全进程预算，模块级采样接入后
    /// 同一入口对 wallpaper 生效）。
    fn degrade_frame_rate(&mut self) {
        let Some(next) = self.fps_tier.degrade_step() else {
            log::error!("wallpaper: budget pressure at lowest tier, suspending to static snapshot");
            let _ = self.suspend_for("budget exceeded (static snapshot)");
            return;
        };
        self.fps_tier = next;
        #[cfg(windows)]
        if let Some(engine) = &self.engine {
            engine.set_frame_interval(next.frame_interval_ms());
        }
        log::warn!("wallpaper: budget pressure, frame rate degraded to {next:?}");
        if matches!(self.state, WallpaperState::Running { .. }) {
            self.state = WallpaperState::Running {
                detail: format!("frame rate degraded to {next:?} (budget pressure)"),
            };
        }
    }

    #[cfg(windows)]
    fn start_engine(&mut self) -> Result<(), ModuleError> {
        // 多显示器 / DPI 摸底（v0.1 §6 专项：先收集拓扑与风险信号）
        let monitors = monitor::probe();
        log::info!("{}", monitors.report_text());

        let topology = topology::probe();
        log::info!("desktop topology: {topology:?}");
        let fallback_color = parse_hex_color(&self.settings.solid_color)
            .unwrap_or_else(|| parse_hex_color("#F76B1C").expect("valid literal"));
        // 内容源组装（P3：配置不可用时降级并有明确说明，不静默黑屏）。
        // 优先级：播放列表当前条目（§13 F1）> 单源 mode
        let content = if let Some(playlist) = &self.playlist {
            match playlist.current_path() {
                Some(path) => content_source_for_path(path, fallback_color),
                None => {
                    log::warn!("wallpaper: playlist empty, solid fallback");
                    renderer::ContentSource::Solid {
                        color: fallback_color,
                    }
                }
            }
        } else {
            match self.settings.mode {
                WallpaperMode::Solid => renderer::ContentSource::Solid {
                    color: fallback_color,
                },
                WallpaperMode::Image => match &self.settings.image_path {
                    Some(path) => renderer::ContentSource::Image {
                        path: path.clone(),
                        fallback_color,
                    },
                    None => {
                        log::warn!("wallpaper: image mode without image_path, solid fallback");
                        renderer::ContentSource::Solid {
                            color: fallback_color,
                        }
                    }
                },
                WallpaperMode::Video => match &self.settings.video_path {
                    Some(path) => renderer::ContentSource::Video {
                        path: path.clone(),
                        fallback_color,
                    },
                    None => {
                        log::warn!("wallpaper: video mode without video_path, solid fallback");
                        renderer::ContentSource::Solid {
                            color: fallback_color,
                        }
                    }
                },
            }
        };
        // 挂载 + DComp 初始化在渲染线程内完成（窗口线程相关性）
        let renderer = renderer::Renderer::spawn(
            topology,
            renderer::RenderConfig {
                content,
                frame_interval_ms: self.fps_tier.frame_interval_ms(),
            },
        )
        .map_err(ModuleError::Other)?;
        self.engine = Some(renderer);
        let detail = match self.settings.mode {
            WallpaperMode::Solid => String::new(),
            WallpaperMode::Image if self.settings.image_path.is_none() => {
                "image mode without image_path, solid fallback".to_string()
            }
            WallpaperMode::Image => String::new(),
            WallpaperMode::Video if self.settings.video_path.is_none() => {
                "video mode without video_path, solid fallback".to_string()
            }
            WallpaperMode::Video => String::new(),
        };
        self.state = WallpaperState::Running { detail };
        Ok(())
    }

    #[cfg(windows)]
    fn stop_engine(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            if let Err(err) = engine.stop() {
                log::warn!("wallpaper renderer stop: {err}");
            }
        }
    }

    /// explorer 重启 / 显示拓扑变化后重新挂载
    #[cfg(windows)]
    fn remount(&mut self) {
        log::info!("wallpaper remount (shell restart / display change)");
        self.stop_engine();
        if let Err(err) = self.start_engine() {
            self.state = WallpaperState::Offline {
                reason: err.to_string(),
            };
        }
    }
}

impl Default for WallpaperModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for WallpaperModule {
    fn declaration(&self) -> ModuleDeclaration {
        // 预算对齐 §4：壁纸播放 <3% CPU / <120MB 内存（空闲时不渲染，接近 0）
        ModuleDeclaration {
            id: "wallpaper",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 3.0,
                max_memory_mb: 120,
                needs_gpu: true,
            },
            description: "wallpaper engine (topology-aware, DirectComposition flip-model)",
        }
    }

    fn initialize(&mut self, config: &ModuleConfig) -> Result<(), ModuleError> {
        self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
        self.fps_tier = self.settings.fps_tier;
        // 播放列表运行态（§13 F1）：种子取启动时刻，保证随机模式不重复开局
        if let Some(playlist) = self.settings.playlist.as_ref() {
            if !playlist.entries.is_empty() {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E3779B97F4A7C15);
                self.minutes_to_switch = playlist.interval_min.max(1);
                self.playlist = Some(PlaylistState::new(playlist, seed));
            }
        }
        #[cfg(windows)]
        {
            self.start_engine()
        }
        #[cfg(not(windows))]
        {
            // Windows 单平台产品（§10）；非 Windows 上走能力探测降级路径，
            // 内核与其他模块照常运行（用于开发机 / CI 验证模块隔离）
            self.state = WallpaperState::Offline {
                reason: "wallpaper requires Windows (DirectComposition / WorkerW)".to_string(),
            };
            Err(ModuleError::Unsupported(
                self.state_offline_reason().to_string(),
            ))
        }
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            // 性能底线：完全暂停而非降帧（§3 A-1）
            KernelEvent::FullscreenChanged { fullscreen: true } => {
                self.suspend_for("fullscreen app")
            }
            KernelEvent::LockScreenChanged { locked: true } => self.suspend_for("lock screen"),
            KernelEvent::PowerSaverChanged { enabled: true } => self.suspend_for("power saver"),
            KernelEvent::FullscreenChanged { fullscreen: false }
            | KernelEvent::LockScreenChanged { locked: false }
            | KernelEvent::PowerSaverChanged { enabled: false } => self.resume_from(),
            // explorer 重启：WorkerW / Progman 层级重建，必须重挂载（模块 C 同款纪律）
            KernelEvent::ShellRestarted | KernelEvent::DisplayChanged => {
                #[cfg(windows)]
                self.remount();
                #[cfg(not(windows))]
                {
                    let _ = event;
                }
                Ok(())
            }
            // §5 降级矩阵：全进程或本模块持续超预算 → 帧率逐档下调直至静态快照
            KernelEvent::BudgetExceeded { module } => {
                if module == "process.total" || module == "wallpaper" {
                    self.degrade_frame_rate();
                }
                Ok(())
            }
            // §13 F1 播放列表：分钟心跳递减，到期切换条目（重启引擎换源）
            KernelEvent::MinuteTick => {
                if let Some(playlist) = self.playlist.as_mut() {
                    if self.minutes_to_switch > 1 {
                        self.minutes_to_switch -= 1;
                    } else {
                        let next_path = playlist.advance().map(|s| s.to_string());
                        let interval = self
                            .settings
                            .playlist
                            .as_ref()
                            .map(|p| p.interval_min.max(1))
                            .unwrap_or(1);
                        self.minutes_to_switch = interval;
                        if let Some(path) = next_path {
                            log::info!("wallpaper: playlist switched to {path}");
                            if matches!(self.state, WallpaperState::Suspended { .. }) {
                                // 暂停中（锁屏 / 全屏 / 节能）：只记账，恢复时再换源
                                self.pending_playlist_switch = true;
                            } else {
                                #[cfg(windows)]
                                {
                                    self.remount();
                                }
                            }
                            #[cfg(not(windows))]
                            {
                                if let WallpaperState::Running { .. } = self.state {
                                    self.state = WallpaperState::Running {
                                        detail: format!("playlist entry: {path}"),
                                    };
                                }
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn suspend(&mut self) -> Result<(), ModuleError> {
        self.suspend_for("kernel suspend_all")
    }

    fn resume(&mut self) -> Result<(), ModuleError> {
        self.resume_from()
    }

    fn finalize(&mut self) -> Result<(), ModuleError> {
        #[cfg(windows)]
        self.stop_engine();
        self.state = WallpaperState::Offline {
            reason: "finalized".to_string(),
        };
        Ok(())
    }

    fn status(&self) -> ModuleStatus {
        // 渲染线程终止（两条降级路径都失败）→ 最高降级档位 + 用户可见原因（P6）
        #[cfg(windows)]
        if let Some(engine) = &self.engine {
            if let Some(err) = engine.render_error() {
                return ModuleStatus {
                    level: DegradeLevel::Suspended,
                    detail: format!("render pipeline failed: {err}"),
                };
            }
        }
        let (level, detail) = match &self.state {
            WallpaperState::Created => (DegradeLevel::Full, String::new()),
            WallpaperState::Running { detail } => (DegradeLevel::Full, detail.clone()),
            WallpaperState::Suspended { reason } => {
                (DegradeLevel::Suspended, format!("suspended: {reason}"))
            }
            WallpaperState::Offline { reason } => (DegradeLevel::Suspended, reason.clone()),
        };
        ModuleStatus { level, detail }
    }
}

impl WallpaperModule {
    fn suspend_for(&mut self, reason: &str) -> Result<(), ModuleError> {
        #[cfg(windows)]
        if let Some(engine) = &self.engine {
            engine.pause_and_trim();
        }
        self.state = WallpaperState::Suspended {
            reason: Self::set_suspend_reason(reason),
        };
        log::info!("wallpaper suspended: {reason}");
        Ok(())
    }

    fn resume_from(&mut self) -> Result<(), ModuleError> {
        if matches!(self.state, WallpaperState::Suspended { .. }) {
            #[cfg(windows)]
            if let Some(engine) = &self.engine {
                engine.unpause();
            }
            self.state = WallpaperState::Running {
                detail: String::new(),
            };
            log::info!("wallpaper resumed");
            // 暂停期间到期的播放列表切换在此补执行（换源后从新条目恢复渲染）
            if self.pending_playlist_switch {
                self.pending_playlist_switch = false;
                #[cfg(windows)]
                self.remount();
                #[cfg(not(windows))]
                log::info!(
                    "wallpaper: pending playlist switch applied ({})",
                    self.playlist
                        .as_ref()
                        .and_then(|p| p.current_path().map(|s| s.to_string()))
                        .unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    // 仅供非 Windows 分支的 Unsupported 原因复用（开发机 / CI 链路验证）
    #[cfg_attr(windows, allow(dead_code))]
    fn state_offline_reason(&self) -> &str {
        match &self.state {
            WallpaperState::Offline { reason } => reason,
            _ => "offline",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_opinionated() {
        // P2 默认即好看：品牌橙 + 60fps
        let settings = WallpaperSettings::default();
        assert_eq!(settings.mode, WallpaperMode::Solid);
        assert_eq!(settings.solid_color, "#F76B1C");
        assert_eq!(settings.fps_tier.frame_interval_ms(), 16);
    }

    #[test]
    fn settings_parse_from_module_config_segment() {
        let config = serde_json::json!({
            "mode": "video",
            "video_path": "C:/w/rain.mp4",
            "fps_tier": "t120"
        });
        let settings: WallpaperSettings = serde_json::from_value(config).unwrap();
        assert_eq!(settings.mode, WallpaperMode::Video);
        assert_eq!(settings.fps_tier.frame_interval_ms(), 8);
        // 缺失字段回默认
        assert_eq!(settings.solid_color, "#F76B1C");
    }

    #[test]
    fn missing_config_segment_uses_defaults() {
        // 与 initialize 同路径：配置段为 Null / 缺失 → unwrap_or_default 兜底（P2）
        let settings = serde_json::from_value::<WallpaperSettings>(serde_json::Value::Null)
            .unwrap_or_default();
        assert_eq!(settings, WallpaperSettings::default());
    }

    #[test]
    fn hex_color_parses() {
        assert_eq!(
            parse_hex_color("#F76B1C"),
            Some([247.0 / 255.0, 107.0 / 255.0, 28.0 / 255.0, 1.0])
        );
        assert_eq!(parse_hex_color("#000000"), Some([0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn hex_color_rejects_garbage() {
        assert_eq!(parse_hex_color("F76B1C"), None);
        assert_eq!(parse_hex_color("#F76B"), None);
        assert_eq!(parse_hex_color("#GGGGGG"), None);
    }

    fn playlist_cfg(
        order: PlaylistOrder,
        entries: &[&str],
        interval_min: u32,
    ) -> WallpaperSettings {
        WallpaperSettings {
            playlist: Some(WallpaperPlaylist {
                entries: entries.iter().map(|s| s.to_string()).collect(),
                interval_min,
                order,
            }),
            ..WallpaperSettings::default()
        }
    }

    #[test]
    fn playlist_sequential_cycles_in_order() {
        let cfg = playlist_cfg(PlaylistOrder::Sequential, &["a.jpg", "b.mp4", "c.png"], 30);
        let mut state = PlaylistState::new(cfg.playlist.as_ref().unwrap(), 1);
        assert_eq!(state.current_path(), Some("a.jpg"));
        assert_eq!(state.advance(), Some("b.mp4"));
        assert_eq!(state.advance(), Some("c.png"));
        assert_eq!(state.advance(), Some("a.jpg"), "wraps around");
    }

    #[test]
    fn playlist_random_is_deterministic_per_seed() {
        let cfg = playlist_cfg(
            PlaylistOrder::Random,
            &["a.jpg", "b.jpg", "c.jpg", "d.jpg"],
            5,
        );
        let mut s1 = PlaylistState::new(cfg.playlist.as_ref().unwrap(), 42);
        let mut s2 = PlaylistState::new(cfg.playlist.as_ref().unwrap(), 42);
        for _ in 0..8 {
            let a = s1.advance().unwrap();
            let b = s2.advance().unwrap();
            assert_eq!(a, b, "same seed walks the same sequence");
        }
        // 不同种子序列大概率不同（弱断言防偶发翻转）
        let mut s3 = PlaylistState::new(cfg.playlist.as_ref().unwrap(), 4242);
        let seq_a: Vec<String> = (0..8).map(|_| s1.advance().unwrap().to_string()).collect();
        let seq_b: Vec<String> = (0..8).map(|_| s3.advance().unwrap().to_string()).collect();
        assert_ne!(seq_a, seq_b);
    }

    #[test]
    fn playlist_empty_and_settings_parse() {
        let state = PlaylistState::new(
            &WallpaperPlaylist {
                entries: vec![],
                interval_min: 10,
                order: PlaylistOrder::Sequential,
            },
            1,
        );
        assert!(!state.is_active());
        assert_eq!(state.current_path(), None);

        let json = serde_json::json!({
            "playlist": {
                "entries": ["C:/w/a.jpg", "D:/videos/rain.mp4"],
                "interval_min": 15,
                "order": "random"
            }
        });
        let settings: WallpaperSettings = serde_json::from_value(json).unwrap();
        let playlist = settings.playlist.unwrap();
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.order, PlaylistOrder::Random);
        assert_eq!(playlist.interval_min, 15);
        // 旧配置（无 playlist 字段）不受影响
        let legacy: WallpaperSettings =
            serde_json::from_value(serde_json::json!({"mode": "solid"})).unwrap();
        assert!(legacy.playlist.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn minute_tick_drives_playlist_switch_with_pending_semantics() {
        let mut module = WallpaperModule::new();
        module.settings = playlist_cfg(PlaylistOrder::Sequential, &["a.jpg", "b.jpg"], 1);
        module.minutes_to_switch = 1;
        module.playlist = Some(PlaylistState::new(
            module.settings.playlist.as_ref().unwrap(),
            1,
        ));
        module.state = WallpaperState::Running {
            detail: String::new(),
        };

        // 到期 tick：切换到 b.jpg（运行态立即生效——非 Windows 下仅更新状态）
        module.on_event(&KernelEvent::MinuteTick).unwrap();
        assert_eq!(
            module.playlist.as_ref().unwrap().current_path(),
            Some("b.jpg")
        );
        assert!(module.status().detail.contains("playlist entry: b.jpg"));
        // interval 重新装填
        assert_eq!(module.minutes_to_switch, 1);

        // 暂停期间到期：只记账不切换状态细节，恢复时补执行
        module.suspend_for("lock screen").unwrap();
        module.on_event(&KernelEvent::MinuteTick).unwrap();
        assert!(module.pending_playlist_switch);
        module
            .on_event(&KernelEvent::LockScreenChanged { locked: false })
            .unwrap();
        assert!(
            !module.pending_playlist_switch,
            "resume consumed the pending switch"
        );
        assert_eq!(
            module.playlist.as_ref().unwrap().current_path(),
            Some("a.jpg")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn initialize_offline_on_non_windows_with_clear_reason() {
        let mut module = WallpaperModule::new();
        let result = module.initialize(&serde_json::Value::Null);
        assert!(
            matches!(result, Err(ModuleError::Unsupported(reason)) if reason.contains("requires Windows"))
        );
        assert_eq!(module.status().level, DegradeLevel::Suspended);
    }

    #[cfg(not(windows))]
    #[test]
    fn suspend_resume_lifecycle_transitions_visible_in_status() {
        let mut module = WallpaperModule::new();
        // 非 Windows：模块 offline，suspend/resume 不 panic
        module.suspend_for("test").unwrap();
        assert!(module.status().detail.contains("suspended: test"));
        module.resume_from().unwrap();
    }

    #[test]
    fn fps_tier_degrade_step_follows_section5_matrix() {
        // §5 矩阵：60 → 30 → 15 → 静态快照（None）；120 / 无限同入 30
        assert_eq!(FpsTier::T60.degrade_step(), Some(FpsTier::T30));
        assert_eq!(FpsTier::T120.degrade_step(), Some(FpsTier::T30));
        assert_eq!(FpsTier::Unlimited.degrade_step(), Some(FpsTier::T30));
        assert_eq!(FpsTier::T30.degrade_step(), Some(FpsTier::T15));
        assert_eq!(FpsTier::T15.degrade_step(), None);
        assert_eq!(FpsTier::T15.frame_interval_ms(), 66);
    }

    #[cfg(not(windows))]
    #[test]
    fn budget_exceeded_walks_the_degradation_chain() {
        let mut module = WallpaperModule::new();
        assert_eq!(module.fps_tier, FpsTier::T60, "default tier");

        // 第一次超标：60 → 30
        module
            .on_event(&KernelEvent::BudgetExceeded {
                module: "process.total".into(),
            })
            .unwrap();
        assert_eq!(module.fps_tier, FpsTier::T30);

        // 第二次：30 → 15
        module
            .on_event(&KernelEvent::BudgetExceeded {
                module: "process.total".into(),
            })
            .unwrap();
        assert_eq!(module.fps_tier, FpsTier::T15);

        // 第三次：已最低档 → 静态快照（Suspended + 用户可见原因）
        module
            .on_event(&KernelEvent::BudgetExceeded {
                module: "process.total".into(),
            })
            .unwrap();
        assert!(module
            .status()
            .detail
            .contains("budget exceeded (static snapshot)"));

        // 其他模块超预算不牵连壁纸
        module
            .on_event(&KernelEvent::BudgetExceeded {
                module: "widgets".into(),
            })
            .unwrap();
    }
}
