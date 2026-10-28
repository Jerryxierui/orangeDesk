//! orangeDesk 声明式皮肤层（§13.2 L2）：一个 YAML 文件 = 一套完整桌面。
//!
//! 设计要点：
//! - **零 schema 漂移**：皮肤各段直接复用模块 crate 的配置类型
//!   （`WallpaperSettings` / `TaskbarSettings` / `WidgetSettings`）——
//!   皮肤就是「模块配置段的 YAML 视图 + 元数据」，模块新增配置字段皮肤自动跟进
//! - **编译为 L1**：`compile()` 把皮肤转成 `modules.<id>` 的 JSON 段，
//!   由既有配置管线消费（内核 / 模块零改动即可被皮肤驱动）
//! - **校验明确**：YAML 解析错误、字段类型错误都给出可读信息（P3 不静默失败）；
//!   `window_material` 段（v0.4 模块）暂存为透传值并在编译时说明
//!
//! 使用：
//! ```text
//! orange-desk --skin my-desk.yaml        # 一次性
//! # 或 config.json: { "active_skin": "my-desk.yaml" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use orange_dock::island::IslandSettings;
use orange_dock::DockSettings;
use orange_taskbar::TaskbarSettings;
use orange_wallpaper::WallpaperSettings;
use orange_widgets::WidgetSettings;

pub mod scenes;
use serde::{Deserialize, Serialize};

/// 皮肤文件根结构（一个文件 = 一套完整桌面）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkinFile {
    pub skin: SkinMeta,
    /// 壁纸段（modules.wallpaper 的 YAML 视图；缺省 = 不覆盖用户配置）
    pub wallpaper: Option<WallpaperSettings>,
    /// 任务栏段（modules.taskbar）
    pub taskbar: Option<TaskbarSettings>,
    /// 组件段（modules.widgets：enabled 开关）
    pub widgets: Option<WidgetSettings>,
    /// 悬浮 Dock 段（modules.dock；F7）
    pub dock: Option<DockSettings>,
    /// 灵动岛段（modules.island）
    pub island: Option<IslandSettings>,
    /// 窗口材质（v0.4 模块预留；仅校验取值，暂不生效并提示）
    pub window_material: Option<WindowMaterialPreview>,
}

/// 皮肤元数据（展示 / 分享用，§6 v1.1 主题包的前身）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkinMeta {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    /// 皮肤 schema 版本（当前 1）
    pub version: u32,
}

impl Default for SkinMeta {
    fn default() -> Self {
        Self {
            name: "unnamed".to_string(),
            author: None,
            description: None,
            version: 1,
        }
    }
}

/// 窗口材质预览段（v0.4 模块的占位；先校验取值让皮肤作者及早发现拼写错误）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMaterialPreview {
    Mica,
    Acrylic,
    None,
}

/// 皮肤层错误（全部用户可读）
#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    #[error("cannot read skin file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("skin file {0} is not valid YAML: {1}")]
    Parse(String, String),
}

/// 从文件加载皮肤（YAML）
pub fn load(path: &Path) -> Result<SkinFile, SkinError> {
    let text = std::fs::read_to_string(path).map_err(|source| SkinError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let skin: SkinFile = serde_yaml::from_str(&text)
        .map_err(|err| SkinError::Parse(path.display().to_string(), err.to_string()))?;
    Ok(skin)
}

/// 编译皮肤为 L1 模块配置段（`modules.<id>` → JSON value）。
///
/// 皮肤中出现的段覆盖用户配置同名段；未出现的段不动（局部换装合法）。
/// 返回 (modules 覆盖集, 诊断信息)：诊断用于日志 / 面板展示（P6）。
pub fn compile(skin: &SkinFile) -> (BTreeMap<String, serde_json::Value>, Vec<String>) {
    let mut modules = BTreeMap::new();
    let mut notes = Vec::new();

    if let Some(meta) = Some(&skin.skin) {
        notes.push(format!(
            "skin '{}' v{} by {}",
            meta.name,
            meta.version,
            meta.author.clone().unwrap_or_default()
        ));
    }
    if let Some(wallpaper) = &skin.wallpaper {
        modules.insert(
            "wallpaper".to_string(),
            serde_json::to_value(wallpaper).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(taskbar) = &skin.taskbar {
        modules.insert(
            "taskbar".to_string(),
            serde_json::to_value(taskbar).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(widgets) = &skin.widgets {
        modules.insert(
            "widgets".to_string(),
            serde_json::to_value(widgets).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(dock) = &skin.dock {
        modules.insert(
            "dock".to_string(),
            serde_json::to_value(dock).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(island) = &skin.island {
        modules.insert(
            "island".to_string(),
            serde_json::to_value(island).unwrap_or(serde_json::Value::Null),
        );
    }
    if skin.window_material.is_some() {
        notes.push(
            "window_material section is reserved for the v0.4 module (parsed, not yet applied)"
                .to_string(),
        );
    }
    (modules, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
skin:
  name: midnight
  author: jerry
  version: 1

wallpaper:
  mode: video
  video_path: D:/walls/rain.mp4
  solid_color: "#101018"
  fps_tier: t60
  playlist:
    entries:
      - D:/walls/dawn.jpg
      - D:/walls/rain.mp4
    interval_min: 15
    order: random

taskbar:
  tint: "#80F76B1C"
  rules:
    - name: game-clear
      kind: fullscreen
      material: clear
    - name: default-acrylic
      kind: always
      material: acrylic

widgets:
  enabled: [clock, sysmon]

window_material: mica
"##;

    #[test]
    fn parses_full_skin_sample() {
        let skin: SkinFile = serde_yaml::from_str(SAMPLE).unwrap();
        assert_eq!(skin.skin.name, "midnight");
        assert_eq!(skin.skin.author.as_deref(), Some("jerry"));

        let wallpaper = skin.wallpaper.unwrap();
        assert_eq!(wallpaper.mode, orange_wallpaper::WallpaperMode::Video);
        assert_eq!(wallpaper.solid_color, "#101018");
        let playlist = wallpaper.playlist.unwrap();
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.order, orange_wallpaper::PlaylistOrder::Random);

        let taskbar = skin.taskbar.unwrap();
        assert_eq!(taskbar.tint, "#80F76B1C");
        assert_eq!(taskbar.rules.as_ref().unwrap().len(), 2);

        assert_eq!(
            skin.widgets.unwrap().enabled,
            Some(vec!["clock".to_string(), "sysmon".to_string()])
        );
        assert_eq!(skin.window_material, Some(WindowMaterialPreview::Mica));
    }

    #[test]
    fn empty_skin_is_valid_and_compiles_to_nothing() {
        // 空皮肤（只有元数据）合法：什么都不覆盖
        let skin: SkinFile = serde_yaml::from_str("skin:\n  name: empty\n").unwrap();
        let (modules, _) = compile(&skin);
        assert!(modules.is_empty());
    }

    #[test]
    fn compile_maps_sections_to_module_configs() {
        let skin: SkinFile = serde_yaml::from_str(SAMPLE).unwrap();
        let (modules, notes) = compile(&skin);
        assert_eq!(modules.len(), 3);
        assert!(modules.contains_key("wallpaper"));
        assert!(modules.contains_key("taskbar"));
        assert!(modules.contains_key("widgets"));
        // 编译产物是合法的 L1 段：能被模块类型重新解析（roundtrip 保证皮肤驱动模块零改动）
        let wallpaper: WallpaperSettings =
            serde_json::from_value(modules["wallpaper"].clone()).unwrap();
        assert_eq!(wallpaper.video_path.as_deref(), Some("D:/walls/rain.mp4"));
        let taskbar: TaskbarSettings = serde_json::from_value(modules["taskbar"].clone()).unwrap();
        assert_eq!(taskbar.tint, "#80F76B1C");
        // v0.4 预留段的提示
        assert!(notes.iter().any(|n| n.contains("window_material")));
    }

    #[test]
    fn bad_yaml_reports_readable_error() {
        let err = load(Path::new("/definitely/not/here.yaml")).unwrap_err();
        assert!(matches!(err, SkinError::Io { .. }));

        let parse_err: Result<SkinFile, _> = serde_yaml::from_str("wallpaper:\n  mode: [broken");
        assert!(parse_err.is_err());
    }

    #[test]
    fn unknown_window_material_is_rejected_early() {
        // 拼写错误在皮肤加载时就报，而不是 v0.4 模块上线后静默无效
        let parsed: Result<SkinFile, _> = serde_yaml::from_str("window_material: glass");
        assert!(parsed.is_err());
    }
}
