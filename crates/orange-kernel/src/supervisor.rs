//! Supervisor：内核启动编排与有序关机。
//!
//! 流程（§2）：
//! 1. 单实例互斥（调用方负责）→ 2. 注册模块 + 依赖解析 →
//! 3. 按拓扑序初始化（失败隔离）→ 4. 启动平台事件源线程 →
//! 5. 事件分发循环 → 6. 收到停机信号后逆序 finalize

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use orange_module_api::{KernelEvent, Module, ModuleConfig};

use crate::budget::BudgetManager;
use crate::bus::EventBus;
use crate::platform;
use crate::registry::ModuleRegistry;

/// supervisor 选项
#[derive(Debug, Clone, Default)]
pub struct SupervisorOptions {
    /// 注入演示事件序列（启动后依次广播环境事件，验证模块响应链路；测试 / CI 用）
    pub demo_events: bool,
}

/// 一次内核运行的报告（退出时打印 / 测试断言用）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunReport {
    pub online_modules: Vec<String>,
    pub offline_modules: Vec<(String, String)>,
    pub crashed_modules: Vec<String>,
}

/// 运行内核直到停机信号（Ctrl+C / ShutdownRequested 事件）。
///
/// 任何模块的失败都不会中断其他模块（P3），返回值汇总最终状态。
pub fn run(
    configs: BTreeMap<String, ModuleConfig>,
    modules: Vec<Box<dyn Module>>,
    options: SupervisorOptions,
) -> RunReport {
    let mut registry = ModuleRegistry::new();
    let mut budgets = BudgetManager::new();
    for module in modules {
        // 预算登记（离线模块也登记，仪表盘统一展示）
        budgets.register(&module.declaration());
        if let Err(err) = registry.register(module) {
            log::error!("module registration failed: {err}");
        }
    }
    match registry.resolve() {
        Ok(_) => {}
        Err(err) => {
            // 成环 / 缺依赖属于打包错误：明确报错但不 panic，已注册的无依赖模块继续
            log::error!("dependency resolution failed, affected modules will stay offline: {err}");
        }
    }
    let online = registry.initialize(&configs);
    log::info!("kernel online: {online} module(s)");

    let bus = EventBus::new();
    platform::install_ctrl_handler()
        .map_err(|err| {
            log::warn!("ctrl handler not installed: {err}");
            err
        })
        .ok();

    // 平台事件源线程
    let mut source = platform::platform_event_source();
    let source_publisher = bus.publisher();
    let source_handle = thread::Builder::new()
        .name("od-event-source".to_string())
        .spawn(move || source.run(source_publisher))
        .expect("spawn event source thread");

    // Startup 广播
    let publisher = bus.publisher();
    registry.dispatch(&KernelEvent::Startup);

    if options.demo_events {
        let demo_publisher = publisher.clone();
        thread::Builder::new()
            .name("od-demo-events".to_string())
            .spawn(move || run_demo_sequence(demo_publisher))
            .expect("spawn demo injector");
    }

    // 事件分发循环（250ms 心跳用于检查停机标志；无高频轮询，P4）
    loop {
        match bus.recv_timeout(Duration::from_millis(250)) {
            Some(KernelEvent::ShutdownRequested) => {
                log::info!("shutdown requested via event bus");
                break;
            }
            Some(event) => registry.dispatch(&event),
            None => {}
        }
        if platform::shutdown_requested() {
            log::info!("shutdown requested via signal");
            break;
        }
    }

    // 有序关机：通知事件源 → 全模块挂起 → 逆序 finalize
    platform::request_shutdown();
    let _ = source_handle.join();
    registry.suspend_all();
    registry.finalize_all();
    log::info!("kernel stopped cleanly");

    let mut report = RunReport::default();
    for state in registry.states() {
        if state.crash_count > 0 {
            report.crashed_modules.push(state.id.clone());
        }
        if state.online {
            report.online_modules.push(state.id.clone());
        } else {
            report
                .offline_modules
                .push((state.id.clone(), state.note.clone()));
        }
    }
    report
}

/// 演示事件序列：验证「事件 → 模块 suspend/resume」链路。
/// 结尾注入 ShutdownRequested 让内核优雅退出（CI 可直接断言全程）。
fn run_demo_sequence(publisher: crate::bus::EventPublisher) {
    use std::time::Instant;
    let started = Instant::now();
    let at = |ms: u64| started + Duration::from_millis(ms);

    let schedule: &[(u64, KernelEvent)] = &[
        (
            300,
            KernelEvent::ForegroundChanged {
                process_name: "demo-app.exe".into(),
            },
        ),
        (600, KernelEvent::FullscreenChanged { fullscreen: true }),
        (1200, KernelEvent::FullscreenChanged { fullscreen: false }),
        (1500, KernelEvent::LockScreenChanged { locked: true }),
        (1900, KernelEvent::LockScreenChanged { locked: false }),
        (2200, KernelEvent::PowerSaverChanged { enabled: true }),
        (2600, KernelEvent::PowerSaverChanged { enabled: false }),
        (2900, KernelEvent::ShellRestarted),
        (3200, KernelEvent::DisplayChanged),
        (4000, KernelEvent::ShutdownRequested),
    ];
    for (ms, event) in schedule {
        let wait = at(*ms).saturating_duration_since(Instant::now());
        thread::sleep(wait);
        log::debug!("demo event: {event:?}");
        publisher.publish(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orange_module_api::{ModuleDeclaration, ModuleError, ModuleStatus, ResourceBudget};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    struct RecordingModule {
        id: &'static str,
        lifecycle: Arc<Mutex<Vec<String>>>,
        counter: Arc<AtomicU32>,
        init_error: Option<ModuleError>,
    }

    impl Module for RecordingModule {
        fn declaration(&self) -> ModuleDeclaration {
            ModuleDeclaration {
                id: self.id,
                depends_on: BTreeSet::new(),
                budget: ResourceBudget {
                    idle_cpu_percent: 1.0,
                    max_memory_mb: 32,
                    needs_gpu: false,
                },
                description: "recording module",
            }
        }
        fn initialize(&mut self, _c: &ModuleConfig) -> Result<(), ModuleError> {
            if let Some(err) = &self.init_error {
                return Err(match err {
                    ModuleError::Unsupported(m) => ModuleError::Unsupported(m.clone()),
                    ModuleError::Crash(m) => ModuleError::Crash(m.clone()),
                    ModuleError::Config(m) => ModuleError::Config(m.clone()),
                    ModuleError::Other(m) => ModuleError::Other(m.clone()),
                });
            }
            self.lifecycle
                .lock()
                .unwrap()
                .push(format!("{}:init", self.id));
            Ok(())
        }
        fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            let name = match event {
                KernelEvent::Startup => "startup",
                KernelEvent::FullscreenChanged { fullscreen: true } => "fullscreen-on",
                KernelEvent::LockScreenChanged { locked: true } => "lock-on",
                KernelEvent::ShutdownRequested => "shutdown",
                _ => "other",
            };
            self.lifecycle
                .lock()
                .unwrap()
                .push(format!("{}:{name}", self.id));
            Ok(())
        }
        fn suspend(&mut self) -> Result<(), ModuleError> {
            self.lifecycle
                .lock()
                .unwrap()
                .push(format!("{}:suspend", self.id));
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ModuleError> {
            Ok(())
        }
        fn finalize(&mut self) -> Result<(), ModuleError> {
            self.lifecycle
                .lock()
                .unwrap()
                .push(format!("{}:finalize", self.id));
            Ok(())
        }
        fn status(&self) -> ModuleStatus {
            ModuleStatus::default()
        }
    }

    #[test]
    fn supervisor_full_lifecycle_with_demo_events() {
        let lifecycle = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicU32::new(0));

        let make = |id: &'static str| RecordingModule {
            id,
            lifecycle: lifecycle.clone(),
            counter: counter.clone(),
            init_error: None,
        };
        let failing = RecordingModule {
            id: "broken",
            lifecycle: lifecycle.clone(),
            counter: AtomicU32::new(0).into(),
            init_error: Some(ModuleError::Unsupported("no DirectX".into())),
        };

        let report = run(
            BTreeMap::new(),
            vec![
                Box::new(make("alpha")),
                Box::new(failing),
                Box::new(make("beta")),
            ],
            SupervisorOptions { demo_events: true },
        );

        assert_eq!(report.online_modules, ["alpha", "beta"]);
        assert_eq!(report.offline_modules.len(), 1);
        assert_eq!(report.offline_modules[0].0, "broken");

        let events = lifecycle.lock().unwrap().clone();
        assert!(events.contains(&"alpha:init".to_string()));
        assert!(events.contains(&"alpha:startup".to_string()));
        assert!(events.contains(&"alpha:fullscreen-on".to_string()));
        assert!(events.contains(&"alpha:lock-on".to_string()));
        assert!(events.contains(&"alpha:suspend".to_string()));
        assert!(events.contains(&"alpha:finalize".to_string()));
        // failed 模块不进入生命周期
        assert!(!events.iter().any(|e| e.starts_with("broken:init")));
    }
}
