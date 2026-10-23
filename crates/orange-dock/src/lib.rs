//! orangeDesk 悬浮 Dock 模块（§13 F7）。
//!
//! 定位（§10 红线内）：**新增**顶层悬浮窗口的启动器 / 运行指示器——
//! 补充任务栏而非替换（不碰 WindHawk / ExplorerPatcher 领地）。
//!
//! 本轮范围：数据模型层——配置（位置/对齐/外观/固定项）、运行状态合并
//! （基名匹配 + 激活指示）、启动命令构造（引号感知分词），全部纯逻辑可测。
//! Windows 渲染窗口（DComp 圆角半透明 + 图标 + 放大动效）、进程枚举、
//! ShellExecuteW 启动 / EnumWindows 激活在真机阶段接入（cfg 骨架就绪）。

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Dock 停靠边
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockPosition {
    Bottom,
    Top,
    Left,
    Right,
}

/// 沿停靠边的对齐
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockAlignment {
    Start,
    Center,
    End,
}

/// Dock 条目
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DockItem {
    /// 固定应用（target = 可执行路径 / shell:URI / 协议链接）
    App {
        name: String,
        target: String,
        /// 启动参数（可选；支持引号分组）
        args: Option<String>,
        /// 自定义图标路径（可选；缺省取可执行文件图标）
        icon: Option<String>,
    },
    /// 分隔符（视觉分组）
    Separator,
}

impl DockItem {
    pub fn app(name: &str, target: &str) -> Self {
        DockItem::App {
            name: name.to_string(),
            target: target.to_string(),
            args: None,
            icon: None,
        }
    }
}

/// Dock 模块配置段（modules.dock / 皮肤 dock 段）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockSettings {
    pub position: DockPosition,
    pub alignment: DockAlignment,
    /// 图标尺寸（px，渲染层消费）
    pub icon_size: u32,
    /// 鼠标离开自动隐藏（渲染层消费）
    pub auto_hide: bool,
    /// 背景不透明度 0..1（渲染层消费）
    pub opacity: f32,
    pub items: Vec<DockItem>,
    /// 显示运行中指示点
    pub indicate_running: bool,
}

impl Default for DockSettings {
    fn default() -> Self {
        Self {
            position: DockPosition::Bottom,
            alignment: DockAlignment::Center,
            icon_size: 48,
            auto_hide: false,
            opacity: 0.85,
            items: Vec::new(),
            indicate_running: true,
        }
    }
}

/// 单条目的运行状态（评估产物；渲染层消费）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockEntryState {
    pub label: String,
    /// 匹配到的运行实例数（0 = 未运行）
    pub running_instances: u32,
    /// 当前前台是否是该应用（激活高亮）
    pub is_active: bool,
}

/// target（路径 / URI）→ 可执行基名（小写）；非路径形态返回 None。
/// shell:URI / 协议链接没有稳定的进程匹配锚点，只做启动。
pub fn exe_basename(target: &str) -> Option<String> {
    // 统一小写后处理（锚点比较用；后缀 .EXE/.exe 均需剥除）
    let path = target.trim().trim_matches('"').to_ascii_lowercase();
    // shell:AppsFolder\... 与 scheme:// 形态跳过
    if path.starts_with("shell:") || path.contains("://") {
        return None;
    }
    let base = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&path)
        .trim_end_matches(".exe");
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// 运行状态合并（纯逻辑）：固定项 × 运行进程（exe 基名，小写）× 前台进程。
/// 分隔符不产出状态行。
pub fn evaluate_entries(
    items: &[DockItem],
    running_exes: &[String],
    foreground_process: &str,
) -> Vec<DockEntryState> {
    let foreground_base = foreground_process
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(foreground_process)
        .to_ascii_lowercase();
    items
        .iter()
        .filter_map(|item| match item {
            DockItem::Separator => None,
            DockItem::App { name, target, .. } => {
                let Some(anchor) = exe_basename(target) else {
                    return Some(DockEntryState {
                        label: name.clone(),
                        running_instances: 0,
                        is_active: false,
                    });
                };
                let running_instances = running_exes
                    .iter()
                    .filter(|exe| {
                        exe.rsplit(['\\', '/'])
                            .next()
                            .unwrap_or(exe)
                            .trim_end_matches(".exe")
                            .to_ascii_lowercase()
                            == anchor
                    })
                    .count() as u32;
                let is_active = !foreground_base.is_empty()
                    && foreground_base
                        .trim_end_matches(".exe")
                        .starts_with(&anchor);
                Some(DockEntryState {
                    label: name.clone(),
                    running_instances,
                    is_active,
                })
            }
        })
        .collect()
}

/// 启动命令（数据层构造；执行在 Windows 侧 ShellExecuteW）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: String,
    pub arguments: Vec<String>,
}

/// 构造启动命令：target 剥引号，args 做引号感知分词。
/// target 为空返回 None。
pub fn build_launch_command(item: &DockItem) -> Option<LaunchCommand> {
    let DockItem::App { target, args, .. } = item else {
        return None;
    };
    let program = target.trim().trim_matches('"').to_string();
    if program.is_empty() {
        return None;
    }
    let arguments = args.as_deref().map(split_args).unwrap_or_default();
    Some(LaunchCommand { program, arguments })
}

/// 引号感知空白分词（支持 "..." 分组与反斜杠转义引号）
pub fn split_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 悬浮 Dock 模块（数据层；渲染窗口真机阶段接入）
pub struct DockModule {
    settings: DockSettings,
    /// 最近一次评估的条目状态（status / 渲染层消费）
    entries: Vec<DockEntryState>,
    /// 前台进程（事件维护）
    foreground: String,
    /// 最近一次运行进程清单（真机由进程枚举刷新；前台变化时据此重估）
    last_running: Vec<String>,
}

impl DockModule {
    pub fn new() -> Self {
        Self {
            settings: DockSettings::default(),
            entries: Vec::new(),
            foreground: String::new(),
            last_running: Vec::new(),
        }
    }

    /// 用外部提供的运行进程清单重估状态（Windows 侧进程枚举接入后调用；
    /// 数据层测试直接驱动）
    pub fn refresh_with(&mut self, running_exes: &[String]) {
        self.last_running = running_exes.to_vec();
        self.entries = evaluate_entries(&self.settings.items, running_exes, &self.foreground);
    }
}

impl Default for DockModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for DockModule {
    fn declaration(&self) -> ModuleDeclaration {
        ModuleDeclaration {
            id: "dock",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 0.5,
                max_memory_mb: 15,
                needs_gpu: true,
            },
            description: "floating dock (launcher entries + running indicators, additive not a taskbar replacement)",
        }
    }

    fn initialize(&mut self, config: &ModuleConfig) -> Result<(), ModuleError> {
        self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
        if self.settings.items.is_empty() {
            // 空 Dock 没有意义：明确降级说明（渲染层上线后用户可见），不误报在线
            log::warn!("dock: no items configured, module idle (add modules.dock.items)");
            return Err(ModuleError::Config("dock has no items".to_string()));
        }
        // TODO(真机): 进程枚举初值 + DComp 顶层窗口创建 + shell hook 订阅
        self.refresh_with(&[]);
        log::info!(
            "dock: {} item(s), {:?}/{:?}",
            self.settings.items.len(),
            self.settings.position,
            self.settings.alignment
        );
        Ok(())
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            KernelEvent::ForegroundChanged { process_name } => {
                self.foreground = process_name.clone();
                let running = self.last_running.clone();
                self.refresh_with(&running);
            }
            KernelEvent::ModuleConfigChanged { config, .. } => {
                self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
                log::info!(
                    "dock: config reloaded ({} item(s))",
                    self.settings.items.len()
                );
                self.refresh_with(&[]);
            }
            KernelEvent::LockScreenChanged { locked: true }
            | KernelEvent::PowerSaverChanged { enabled: true } => {
                // 渲染层真机阶段：锁屏 / 节能隐藏 Dock（数据层无操作）
            }
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
        let running = self
            .entries
            .iter()
            .filter(|e| e.running_instances > 0)
            .count();
        ModuleStatus {
            level: DegradeLevel::Full,
            detail: format!("{} item(s), {} running", self.entries.len(), running),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_basename_extracts_anchor() {
        assert_eq!(
            exe_basename("C:\\Program Files\\App\\Code.exe"),
            Some("code".to_string())
        );
        assert_eq!(
            exe_basename("\"D:\\Tools\\CS2.EXE\""),
            Some("cs2".to_string())
        );
        assert_eq!(exe_basename("plain.exe"), Some("plain".to_string()));
        assert_eq!(exe_basename("shell:AppsFolder\\A.B!C"), None);
        assert_eq!(exe_basename("https://example.com"), None);
        assert_eq!(exe_basename(""), None);
    }

    #[test]
    fn evaluate_merges_running_and_active() {
        let items = vec![
            DockItem::app("VS Code", "C:/Apps/Code.exe"),
            DockItem::app("Terminal", "C:/Windows/system32/wt.exe"),
            DockItem::Separator,
            DockItem::app("Steam", "steam://store"),
        ];
        let running = vec![
            "C:\\Apps\\Code.exe".to_string(),
            "C:\\Apps\\code.exe".to_string(), // 大小写不敏感计 2 实例
            "C:\\Windows\\System32\\WT.exe".to_string(),
        ];
        let states = evaluate_entries(&items, &running, "C:\\Apps\\Code.exe");
        assert_eq!(states.len(), 3, "separator produces no row");
        assert_eq!(states[0].label, "VS Code");
        assert_eq!(states[0].running_instances, 2);
        assert!(states[0].is_active, "foreground code matches");
        assert_eq!(states[1].running_instances, 1);
        assert!(!states[1].is_active);
        assert_eq!(states[2].running_instances, 0, "URI entries never match");
        assert!(!states[2].is_active);
    }

    #[test]
    fn launch_command_quotes_and_args() {
        let item = DockItem::App {
            name: "Editor".into(),
            target: "\"C:/Program Files/Editor/editor.exe\"".into(),
            args: Some("-o \"my file.txt\" --flag".into()),
            icon: None,
        };
        let cmd = build_launch_command(&item).unwrap();
        assert_eq!(cmd.program, "C:/Program Files/Editor/editor.exe");
        assert_eq!(
            cmd.arguments,
            vec![
                "-o".to_string(),
                "my file.txt".to_string(),
                "--flag".to_string()
            ]
        );
        // 分隔符与空 target 拒绝
        assert!(build_launch_command(&DockItem::Separator).is_none());
        assert!(build_launch_command(&DockItem::app("x", "  ")).is_none());
    }

    #[test]
    fn split_args_handles_escapes_and_empty() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("a b"), vec!["a", "b"]);
        assert_eq!(split_args("\"a b\" c"), vec!["a b", "c"]);
        assert_eq!(
            split_args("\"quote: \\\"inside\\\"\""),
            vec!["quote: \"inside\""]
        );
        assert_eq!(split_args("  spaced   out  "), vec!["spaced", "out"]);
    }

    #[test]
    fn settings_parse_from_config() {
        let json = serde_json::json!({
            "position": "left",
            "alignment": "start",
            "icon_size": 40,
            "opacity": 0.6,
            "auto_hide": true,
            "indicate_running": false,
            "items": [
                { "kind": "app", "name": "Explorer", "target": "C:/Windows/explorer.exe" },
                { "kind": "separator" },
                { "kind": "app", "name": "Store", "target": "ms-windows-store://" }
            ]
        });
        let settings: DockSettings = serde_json::from_value(json).unwrap();
        assert_eq!(settings.position, DockPosition::Left);
        assert_eq!(settings.alignment, DockAlignment::Start);
        assert_eq!(settings.items.len(), 3);
        assert!(!settings.indicate_running);

        // 空配置 = 默认（P2；模块 initialize 同路径 unwrap_or_default）
        let empty: DockSettings =
            serde_json::from_value(serde_json::Value::Null).unwrap_or_default();
        assert_eq!(empty, DockSettings::default());
    }

    #[test]
    fn module_without_items_reports_config_error() {
        let mut module = DockModule::new();
        assert!(matches!(
            module.initialize(&serde_json::Value::Null),
            Err(ModuleError::Config(_))
        ));
    }

    #[test]
    fn module_tracks_foreground_and_reports_status() {
        let mut module = DockModule::new();
        module.settings.items = vec![DockItem::app("Code", "C:/Apps/Code.exe")];
        module.refresh_with(&["C:\\Apps\\Code.exe".to_string()]);
        assert_eq!(module.entries[0].running_instances, 1);
        assert!(!module.entries[0].is_active);
        module
            .on_event(&KernelEvent::ForegroundChanged {
                process_name: "code.exe".into(),
            })
            .unwrap();
        // 前台基名前缀匹配锚点 → 激活（refresh_with([]) 保留前台信息）
        assert!(module.entries[0].is_active);
        assert!(module.status().detail.contains("1 running"));
    }
}
