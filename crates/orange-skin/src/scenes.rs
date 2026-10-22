//! 场景模式（§13.2 L3 / F3b）：命名的皮肤组合 + 触发器自动切换。
//!
//! 一个场景 = 一套完整的桌面配置（壁纸 / 任务栏 / 组件段，复用 L2 类型）；
//! 场景包 = 多个场景 + 触发器（前台应用匹配、每日时间表）+ 当前活动场景。
//!
//! 评估与应用都是纯逻辑（可测）：supervisor 在 ForegroundChanged / MinuteTick
//! 时评估触发器，命中后把目标场景编译为 `ModuleConfigChanged` 段下发——
//! 模块热应用，进程不重启（配置热重载共用同一路径）。
//!
//! 使用：
//! ```text
//! orange-desk --scene game          # 手动切换并持久化（config.json active_scene）
//! # 触发器在 scenes.yaml 定义（默认路径 <config_dir>/scenes.yaml）
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use orange_taskbar::TaskbarSettings;
use orange_wallpaper::WallpaperSettings;
use orange_widgets::WidgetSettings;
use serde::{Deserialize, Serialize};

use crate::SkinError;

/// 场景包（scenes.yaml 的根）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenePack {
    /// 场景名 → 场景内容
    pub scenes: BTreeMap<String, Scene>,
    /// 触发器（顺序无关；命中即切换）
    pub triggers: Vec<Trigger>,
    /// 启动时激活的场景（CLI --scene 会覆盖并持久化回这里）
    pub active: Option<String>,
}

/// 单个场景：出现 的段在切换时下发，缺省段保持现状
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scene {
    pub wallpaper: Option<WallpaperSettings>,
    pub taskbar: Option<TaskbarSettings>,
    pub widgets: Option<WidgetSettings>,
}

/// 场景触发器
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Trigger {
    /// 前台进程匹配（对路径基名大小写不敏感子串匹配，任一命中即触发）
    Foreground {
        scene: String,
        patterns: Vec<String>,
    },
    /// 每日时间表（HH:MM，本地时间；MinuteTick 对齐检查）
    Schedule { scene: String, at: String },
}

/// 场景包文件加载（YAML）
pub fn load_pack(path: &Path) -> Result<ScenePack, SkinError> {
    let text = std::fs::read_to_string(path).map_err(|source| SkinError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_yaml::from_str(&text)
        .map_err(|err| SkinError::Parse(path.display().to_string(), err.to_string()))
}

/// 校验场景包：触发器引用的场景必须存在、时间表格式合法。
/// 返回诊断列表（空 = 通过）；诊断为错不异常——加载失败优于静默错切（P3）。
pub fn validate_pack(pack: &ScenePack) -> Vec<String> {
    let mut issues = Vec::new();
    for trigger in &pack.triggers {
        let (scene, detail) = match trigger {
            Trigger::Foreground { scene, patterns } => {
                let detail = if patterns.iter().any(|p| p.trim().is_empty()) {
                    "contains empty pattern".to_string()
                } else {
                    String::new()
                };
                (scene, detail)
            }
            Trigger::Schedule { scene, at } => {
                let detail = if parse_hhmm(at).is_none() {
                    format!("schedule time '{at}' is not HH:MM")
                } else {
                    String::new()
                };
                (scene, detail)
            }
        };
        if !pack.scenes.contains_key(scene) {
            issues.push(format!("trigger references unknown scene '{scene}'"));
        }
        if !detail.is_empty() {
            issues.push(format!("trigger for scene '{scene}': {detail}"));
        }
    }
    if let Some(active) = &pack.active {
        if !pack.scenes.contains_key(active) {
            issues.push(format!("active scene '{active}' not defined"));
        }
    }
    issues
}

/// 前台触发器评估：返回命中的目标场景（多个命中取第一个定义的）
pub fn match_foreground(pack: &ScenePack, foreground_process: &str) -> Option<String> {
    let base = foreground_process
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(foreground_process)
        .to_ascii_lowercase();
    if base.is_empty() {
        return None;
    }
    pack.triggers.iter().find_map(|trigger| match trigger {
        Trigger::Foreground { scene, patterns } => {
            let hit = patterns
                .iter()
                .any(|p| !p.trim().is_empty() && base.contains(&p.to_ascii_lowercase()));
            hit.then(|| scene.clone())
        }
        Trigger::Schedule { .. } => None,
    })
}

/// 时间表触发器评估：当前本地 HH:MM 精确匹配（MinuteTick 分钟粒度）
pub fn match_schedule(pack: &ScenePack, minute_of_day: u32) -> Option<String> {
    let now = format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60);
    pack.triggers.iter().find_map(|trigger| match trigger {
        Trigger::Schedule { scene, at } => (at == &now).then(|| scene.clone()),
        Trigger::Foreground { .. } => None,
    })
}

/// 应用场景：编译为待下发的模块配置段（module id → 新配置）
pub fn compile_scene(scene: &Scene) -> BTreeMap<String, serde_json::Value> {
    let mut modules = BTreeMap::new();
    if let Some(wallpaper) = &scene.wallpaper {
        modules.insert(
            "wallpaper".to_string(),
            serde_json::to_value(wallpaper).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(taskbar) = &scene.taskbar {
        modules.insert(
            "taskbar".to_string(),
            serde_json::to_value(taskbar).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(widgets) = &scene.widgets {
        modules.insert(
            "widgets".to_string(),
            serde_json::to_value(widgets).unwrap_or(serde_json::Value::Null),
        );
    }
    modules
}

/// 解析 "HH:MM" → 当日分钟数
pub fn parse_hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// 当前本地时间（当日分钟数）；Schedule 触发器的对齐基准
pub fn local_minute_of_day() -> u32 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_yaml() -> String {
        r##"
scenes:
  work:
    wallpaper: { mode: image, image_path: C:/walls/focus.jpg, fps_tier: t30 }
    taskbar: { tint: "#CC000000" }
    widgets: { enabled: [clock, sysmon] }
  game:
    wallpaper: { mode: solid, solid_color: "#000000" }
    taskbar:
      rules:
        - { name: game, kind: always, material: clear }
    widgets: { enabled: [clock] }
  night:
    wallpaper: { mode: video, video_path: D:/walls/fire.mp4 }
active: work

triggers:
  - { kind: foreground, scene: game, patterns: ["Cyberpunk2077", "cs2.exe"] }
  - { kind: schedule, scene: night, at: "22:30" }
"##
        .to_string()
    }

    #[test]
    fn parses_pack_and_compiles_scene() {
        let pack: ScenePack = serde_yaml::from_str(&pack_yaml()).unwrap();
        assert_eq!(pack.active.as_deref(), Some("work"));
        assert_eq!(pack.scenes.len(), 3);
        assert!(validate_pack(&pack).is_empty());

        let modules = compile_scene(&pack.scenes["game"]);
        assert_eq!(modules.len(), 3);
        let wallpaper: WallpaperSettings =
            serde_json::from_value(modules["wallpaper"].clone()).unwrap();
        assert_eq!(wallpaper.solid_color, "#000000");
        let widgets: WidgetSettings = serde_json::from_value(modules["widgets"].clone()).unwrap();
        assert_eq!(widgets.enabled, Some(vec!["clock".to_string()]));
    }

    #[test]
    fn foreground_trigger_matches_basename_case_insensitive() {
        let pack: ScenePack = serde_yaml::from_str(&pack_yaml()).unwrap();
        assert_eq!(
            match_foreground(&pack, "C:\\Games\\CP2077\\Cyberpunk2077.exe"),
            Some("game".to_string())
        );
        assert_eq!(match_foreground(&pack, "cs2.exe"), Some("game".to_string()));
        assert_eq!(match_foreground(&pack, "explorer.exe"), None);
        assert_eq!(match_foreground(&pack, ""), None);
    }

    #[test]
    fn schedule_trigger_matches_minute_of_day() {
        let pack: ScenePack = serde_yaml::from_str(&pack_yaml()).unwrap();
        assert_eq!(
            match_schedule(&pack, 22 * 60 + 30),
            Some("night".to_string())
        );
        assert_eq!(match_schedule(&pack, 22 * 60 + 31), None);
        assert_eq!(match_schedule(&pack, 0), None);
    }

    #[test]
    fn validation_reports_unknown_scene_and_bad_time() {
        let bad: ScenePack = serde_yaml::from_str(
            "scenes:\n  a: {}\ntriggers:\n  - { kind: schedule, scene: ghost, at: \"9:99\" }\nactive: missing\n",
        )
        .unwrap();
        let issues = validate_pack(&bad);
        assert!(issues.iter().any(|i| i.contains("unknown scene 'ghost'")));
        assert!(issues.iter().any(|i| i.contains("not HH:MM")));
        assert!(issues.iter().any(|i| i.contains("active scene 'missing'")));
    }

    #[test]
    fn hhmm_parsing_roundtrip() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("22:30"), Some(1350));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("9:30"), None, "two-digit fields required");
        assert_eq!(parse_hhmm("12:60"), None);
    }
}
