//! orangeDesk 任务栏透明模块（§3 模块 C：调节器，不是替换器）。
//!
//! - 主路径：`SetWindowCompositionAttribute` + ACCENT_POLICY（动态解析，undocumented）
//! - 纯事件驱动（P4）：订阅 Foreground / Fullscreen / PowerSaver / LockScreen /
//!   ShellRestarted / DisplayChanged，无任何轮询；空闲 CPU < 0.5%、内存 < 15MB
//! - explorer 重启 → 重找任务栏窗口并重新应用（句柄失效）
//! - 能力探测降级链：Acrylic → Blur → Clear → Native，原因在 status 用户可见
//! - 规则引擎：声明式「条件 → 材质」，默认规则集开箱即用（P2），
//!   配置 `modules.taskbar.rules` 可整体覆盖
//! - finalize 恢复 Native（P3 卸载零残留）

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub mod rules;

#[cfg(windows)]
mod composition;

use rules::{EnvState, Material, Rule, RuleEngine};

/// 模块配置段（modules.taskbar），缺失时全部取默认值（P2）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskbarSettings {
    /// 自定义规则集（整体替换内置默认；空列表等价于默认）
    pub rules: Option<Vec<Rule>>,
    /// 着色（#RRGGBB / #RRGGBBAA），默认 80% 黑（Acrylic 常见观感）
    pub tint: String,
}

impl Default for TaskbarSettings {
    fn default() -> Self {
        Self {
            rules: None,
            tint: "#CC000000".to_string(),
        }
    }
}

/// 任务栏透明模块
pub struct TaskbarModule {
    settings: TaskbarSettings,
    engine: RuleEngine,
    env: EnvState,
    /// 当前已应用（含能力降级后实际生效）的材质
    current: Material,
    #[cfg(windows)]
    composition: Option<composition::Composition>,
    /// 任务栏窗口句柄（主 + 多显示器次要）
    #[cfg(windows)]
    taskbar_windows: Vec<windows::Win32::Foundation::HWND>,
}

// SAFETY: HWND 为系统全局句柄（数值），模块在内核分发线程创建与使用；
// Box<dyn Module: Send> 的边界要求显式声明。
unsafe impl Send for TaskbarModule {}

impl TaskbarModule {
    pub fn new() -> Self {
        Self {
            settings: TaskbarSettings::default(),
            engine: RuleEngine::default(),
            env: EnvState::default(),
            current: Material::Native,
            #[cfg(windows)]
            composition: None,
            #[cfg(windows)]
            taskbar_windows: Vec::new(),
        }
    }

    /// 事件后重新评估目标材质；变化时应用到所有任务栏窗口。
    /// 应用内部带能力探测降级链，实际生效材质可能低于目标（记录原因）。
    #[cfg(windows)]
    fn reevaluate(&mut self) {
        let target = self.engine.evaluate(&self.env);
        if target == self.current && !self.taskbar_windows.is_empty() {
            return;
        }
        self.apply(target);
    }

    #[cfg(windows)]
    fn apply(&mut self, target: Material) {
        let Some(composition) = self.composition.as_mut() else {
            return;
        };
        let tint = rules::parse_tint_abgr(&self.settings.tint).unwrap_or(0xCC000000);
        let mut effective = Material::Native;
        for hwnd in &self.taskbar_windows {
            effective = composition.apply(*hwnd, target, tint);
        }
        if effective != target {
            log::warn!(
                "taskbar: {target:?} degraded to {effective:?} ({})",
                composition.last_note
            );
        } else {
            log::info!("taskbar: applied {target:?}");
        }
        self.current = effective;
    }

    /// explorer 重启 / 显示变化后重找任务栏窗口并重应用当前材质
    #[cfg(windows)]
    fn relocate(&mut self) {
        self.taskbar_windows = composition::locate_taskbar_windows();
        if self.taskbar_windows.is_empty() {
            log::warn!("taskbar: no taskbar window found (Shell_TrayWnd missing)");
            return;
        }
        log::info!("taskbar: tracking {} window(s)", self.taskbar_windows.len());
        let target = self.engine.evaluate(&self.env);
        self.apply(target);
    }
}

impl Default for TaskbarModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for TaskbarModule {
    fn declaration(&self) -> ModuleDeclaration {
        // 预算对齐 §4：任务栏模块空闲 CPU < 0.5%、内存 < 15MB、无 GPU
        ModuleDeclaration {
            id: "taskbar",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 0.5,
                max_memory_mb: 15,
                needs_gpu: false,
            },
            description: "taskbar transparency (SetWindowCompositionAttribute, rule-driven)",
        }
    }

    fn initialize(&mut self, config: &ModuleConfig) -> Result<(), ModuleError> {
        self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
        let rules = self
            .settings
            .rules
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(RuleEngine::default_rules);
        self.engine = RuleEngine::new(rules);

        #[cfg(windows)]
        {
            self.composition = Some(composition::Composition::new());
            self.env = EnvState::default();
            self.relocate();
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = &self.settings;
            Err(ModuleError::Unsupported(
                "taskbar module requires Windows (SetWindowCompositionAttribute / Shell_TrayWnd)"
                    .to_string(),
            ))
        }
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            KernelEvent::ForegroundChanged { process_name } => {
                self.env.foreground_process = process_name.clone();
                #[cfg(windows)]
                self.reevaluate();
                Ok(())
            }
            KernelEvent::FullscreenChanged { fullscreen } => {
                self.env.fullscreen = *fullscreen;
                #[cfg(windows)]
                self.reevaluate();
                Ok(())
            }
            KernelEvent::PowerSaverChanged { enabled } => {
                self.env.power_saver = *enabled;
                #[cfg(windows)]
                self.reevaluate();
                Ok(())
            }
            KernelEvent::LockScreenChanged { locked } => {
                self.env.locked = *locked;
                #[cfg(windows)]
                self.reevaluate();
                Ok(())
            }
            KernelEvent::ShellRestarted => {
                // explorer 重启：任务栏窗口句柄失效，必须重找并重应用
                #[cfg(windows)]
                self.relocate();
                #[cfg(not(windows))]
                {
                    let _ = event;
                }
                Ok(())
            }
            KernelEvent::DisplayChanged => {
                // 显示拓扑 / DPI 变化：重应用当前材质（任务栏被重建的可能）
                #[cfg(windows)]
                self.relocate();
                #[cfg(not(windows))]
                {
                    let _ = event;
                }
                Ok(())
            }
            // 热配置（§13 L3 / F3b）：场景切换 → 重建规则集并立即重评估
            KernelEvent::ModuleConfigChanged { config, .. } => {
                self.settings = serde_json::from_value(config.clone()).unwrap_or_default();
                let rules = self
                    .settings
                    .rules
                    .clone()
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(RuleEngine::default_rules);
                self.engine = RuleEngine::new(rules);
                log::info!(
                    "taskbar: rules reloaded ({} rule(s))",
                    self.engine.rules().len()
                );
                #[cfg(windows)]
                self.reevaluate();
                #[cfg(not(windows))]
                {
                    let _ = event;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn suspend(&mut self) -> Result<(), ModuleError> {
        // 事件驱动模块无后台线程；保持当前材质（退出路径由 finalize 恢复 Native）
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ModuleError> {
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), ModuleError> {
        // P3 卸载零残留：恢复系统原生任务栏
        #[cfg(windows)]
        {
            let tint = rules::parse_tint_abgr(&self.settings.tint).unwrap_or(0xCC000000);
            if let Some(composition) = self.composition.as_mut() {
                for hwnd in &self.taskbar_windows {
                    composition.apply(*hwnd, Material::Native, tint);
                }
            }
            self.current = Material::Native;
        }
        Ok(())
    }

    fn status(&self) -> ModuleStatus {
        let note = cfg!(windows)
            .then(|| {
                #[cfg(windows)]
                {
                    self.composition
                        .as_ref()
                        .map(|c| c.last_note.clone())
                        .unwrap_or_default()
                }
                #[cfg(not(windows))]
                {
                    String::new()
                }
            })
            .unwrap_or_default();
        ModuleStatus {
            level: DegradeLevel::Full,
            detail: if note.is_empty() {
                format!("material: {:?}", self.current)
            } else {
                format!("material: {:?} — {note}", self.current)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_opinionated() {
        let settings = TaskbarSettings::default();
        assert_eq!(settings.tint, "#CC000000");
        assert!(
            settings.rules.is_none(),
            "no custom rules = built-in defaults"
        );
    }

    #[test]
    fn settings_parse_custom_rules() {
        let config = serde_json::json!({
            "tint": "#80F76B1C",
            "rules": [
                { "name": "editor", "kind": "foregroundprocess", "pattern": "code", "material": "clear" },
                { "name": "rest", "kind": "always", "material": "blur" }
            ]
        });
        let settings: TaskbarSettings = serde_json::from_value(config).unwrap();
        let engine = RuleEngine::new(
            settings
                .rules
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(RuleEngine::default_rules),
        );
        let mut env = EnvState {
            foreground_process: "C:\\tools\\Code.exe".into(),
            ..Default::default()
        };
        assert_eq!(engine.evaluate(&env), Material::Clear);
        env.foreground_process = "explorer.exe".into();
        assert_eq!(engine.evaluate(&env), Material::Blur);
    }

    #[test]
    fn empty_rules_list_falls_back_to_defaults() {
        // 空 rules 列表 = 没有自定义 → 内置默认（P2：不因空配置而失去默认效果）
        let config = serde_json::json!({ "rules": [] });
        let settings: TaskbarSettings = serde_json::from_value(config).unwrap();
        let rules = settings
            .rules
            .filter(|r| !r.is_empty())
            .unwrap_or_else(RuleEngine::default_rules);
        let engine = RuleEngine::new(rules);
        assert_eq!(engine.evaluate(&EnvState::default()), Material::Acrylic);
    }

    #[test]
    fn missing_config_segment_uses_defaults() {
        let settings: TaskbarSettings =
            serde_json::from_value(serde_json::Value::Null).unwrap_or_default();
        assert_eq!(settings, TaskbarSettings::default());
    }

    #[cfg(not(windows))]
    #[test]
    fn initialize_offline_on_non_windows_with_clear_reason() {
        let mut module = TaskbarModule::new();
        let result = module.initialize(&serde_json::Value::Null);
        assert!(
            matches!(result, Err(ModuleError::Unsupported(reason)) if reason.contains("requires Windows"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn events_update_env_state_without_panicking() {
        // 非 Windows：事件仍累积环境状态（规则引擎可测链路），apply 路径被 cfg 隔离
        let mut module = TaskbarModule::new();
        module
            .on_event(&KernelEvent::ForegroundChanged {
                process_name: "game.exe".into(),
            })
            .unwrap();
        assert_eq!(module.env.foreground_process, "game.exe");
        module
            .on_event(&KernelEvent::FullscreenChanged { fullscreen: true })
            .unwrap();
        assert!(module.env.fullscreen);
        assert_eq!(
            module.engine.evaluate(&module.env),
            Material::Clear,
            "fullscreen rule drives material even before apply is possible"
        );
    }

    #[test]
    fn status_mentions_current_material() {
        let module = TaskbarModule::new();
        assert!(module.status().detail.contains("Native"));
    }
}
