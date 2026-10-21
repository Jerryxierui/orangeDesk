//! 模块注册中心：依赖解析（拓扑排序）、生命周期编排、崩溃隔离（§2）。
//!
//! 纪律落地点：
//! - 依赖成环 / 缺失 → 注册错误，内核拒绝启动该链路
//! - 初始化失败 → 该模块 offline，依赖它的模块级联 offline（不 panic、不白屏）
//! - 模块 panic / 返回 `Crash` → 隔离（停止向其分发事件），其他模块无感
//! - finalize 按初始化逆序执行，失败仅记录日志

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};

use orange_module_api::{KernelEvent, Module, ModuleConfig, ModuleStatus};

/// 单模块运行时状态（日志与性能仪表盘的数据源）
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleState {
    pub id: String,
    /// 是否成功初始化并在线
    pub online: bool,
    pub suspended: bool,
    pub crash_count: u32,
    pub status: ModuleStatus,
    /// offline / 崩溃原因
    pub note: String,
}

/// 注册中心错误
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("duplicate module id: {0}")]
    Duplicate(String),
    #[error("dependency cycle involving: {0:?}")]
    Cycle(BTreeSet<String>),
    #[error("module {0} depends on missing module {1}")]
    MissingDependency(String, String),
    #[error("module {0} depends on itself")]
    SelfDependency(String),
}

/// 模块注册与生命周期编排中心。
pub struct ModuleRegistry {
    modules: Vec<Box<dyn Module>>,
    /// 按拓扑序存放 `modules` 的下标（依赖在前）
    order: Vec<usize>,
    states: BTreeMap<String, usize>,
    state_view: BTreeMap<String, ModuleState>,
    initialized: bool,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
            order: Vec::new(),
            states: BTreeMap::new(),
            state_view: BTreeMap::new(),
            initialized: false,
        }
    }

    /// 注册模块。重复 id 报错。
    pub fn register(&mut self, module: Box<dyn Module>) -> Result<(), RegistryError> {
        let decl = module.declaration();
        if self.states.contains_key(decl.id) {
            return Err(RegistryError::Duplicate(decl.id.to_string()));
        }
        if decl.depends_on.contains(decl.id) {
            return Err(RegistryError::SelfDependency(decl.id.to_string()));
        }
        log::info!("registered module '{}' ({})", decl.id, decl.description);
        self.state_view.insert(
            decl.id.to_string(),
            ModuleState {
                id: decl.id.to_string(),
                online: false,
                suspended: false,
                crash_count: 0,
                status: ModuleStatus::default(),
                note: "registered".to_string(),
            },
        );
        self.states.insert(decl.id.to_string(), self.modules.len());
        self.modules.push(module);
        Ok(())
    }

    /// 解析依赖并计算拓扑序（Kahn）。必须在 initialize 前调用一次。
    pub fn resolve(&mut self) -> Result<&[usize], RegistryError> {
        let ids: BTreeSet<String> = self.states.keys().cloned().collect();
        // 校验依赖存在性
        for module in &self.modules {
            let decl = module.declaration();
            for dep in &decl.depends_on {
                if !ids.contains(*dep) {
                    return Err(RegistryError::MissingDependency(
                        decl.id.to_string(),
                        dep.to_string(),
                    ));
                }
            }
        }

        // Kahn 拓扑排序：节点 = modules 下标，边 = dep -> module
        let n = self.modules.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (idx, module) in self.modules.iter().enumerate() {
            for dep in &module.declaration().depends_on {
                let dep_idx = self.states[*dep];
                indegree[idx] += 1;
                dependents[dep_idx].push(idx);
            }
        }

        let mut queue: VecDeque<usize> = indegree
            .iter()
            .enumerate()
            .filter(|(_, d)| **d == 0)
            .map(|(i, _)| i)
            .collect();
        let mut order = Vec::with_capacity(n);
        while let Some(idx) = queue.pop_front() {
            order.push(idx);
            for &next in &dependents[idx] {
                indegree[next] -= 1;
                if indegree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        if order.len() != n {
            let cyclic: BTreeSet<String> = indegree
                .iter()
                .enumerate()
                .filter(|(_, d)| **d > 0)
                .map(|(i, _)| self.modules[i].declaration().id.to_string())
                .collect();
            return Err(RegistryError::Cycle(cyclic));
        }

        self.order = order;
        Ok(&self.order)
    }

    /// 按拓扑序初始化所有模块，返回在线模块数。
    ///
    /// 失败语义（P3）：某模块初始化失败 → offline；依赖它的模块级联 offline。
    pub fn initialize(&mut self, configs: &BTreeMap<String, ModuleConfig>) -> usize {
        assert!(!self.initialized, "initialize called twice");
        self.initialized = true;
        let order = self.order.clone();
        let mut online = 0usize;
        for idx in order {
            let decl_id = self.modules[idx].declaration().id.to_string();
            // 依赖是否全部在线
            let deps: Vec<String> = self.modules[idx]
                .declaration()
                .depends_on
                .iter()
                .map(|s| s.to_string())
                .collect();
            let offline_dep = deps.iter().find(|d| !self.state_view[*d].online).cloned();
            if let Some(dep) = offline_dep {
                self.mark_offline(&decl_id, format!("dependency offline: {dep}"));
                continue;
            }
            let empty = serde_json::Value::Null;
            let config = configs.get(&decl_id).unwrap_or(&empty);
            let result = catch_unwind(AssertUnwindSafe(|| self.modules[idx].initialize(config)));
            match result {
                Ok(Ok(())) => {
                    let state = self.state_view.get_mut(&decl_id).expect("state exists");
                    state.online = true;
                    state.note = String::new();
                    state.status = self.modules[idx].status();
                    online += 1;
                    log::info!("module '{decl_id}' initialized (online)");
                }
                Ok(Err(err)) => {
                    // Unsupported → 能力探测降级；Config/Other 初始化错误同样只 offline 本模块
                    self.mark_offline(&decl_id, err.to_string());
                }
                Err(panic) => {
                    let reason = panic_message(&panic);
                    self.mark_crashed(&decl_id, &format!("panic during initialize: {reason}"));
                }
            }
        }
        online
    }

    /// 分发事件到所有在线且未崩溃的模块。单模块 panic / Crash → 隔离。
    pub fn dispatch(&mut self, event: &KernelEvent) {
        let order = self.order.clone();
        for idx in order {
            let id = self.modules[idx].declaration().id.to_string();
            if !self.state_view[&id].online {
                continue;
            }
            let result = catch_unwind(AssertUnwindSafe(|| self.modules[idx].on_event(event)));
            match result {
                Ok(Ok(())) => {
                    // 事件处理可能改变模块降级档位，刷新快照供仪表盘读取
                    let status = self.modules[idx].status();
                    self.state_view.get_mut(&id).expect("state exists").status = status;
                }
                Ok(Err(orange_module_api::ModuleError::Crash(msg))) => {
                    self.mark_crashed(&id, &msg);
                }
                Ok(Err(err)) => {
                    log::warn!("module '{id}' event error ({event:?}): {err}");
                    let state = self.state_view.get_mut(&id).expect("state exists");
                    state.note = err.to_string();
                }
                Err(panic) => {
                    let reason = panic_message(&panic);
                    self.mark_crashed(&id, &format!("panic in on_event: {reason}"));
                }
            }
        }
    }

    /// 挂起所有在线模块（进入锁屏 / 电池节能等全局收缩场景由事件驱动，模块自行订阅）
    pub fn suspend_all(&mut self) {
        let order = self.order.clone();
        for idx in order {
            let id = self.modules[idx].declaration().id.to_string();
            if !self.state_view[&id].online || self.state_view[&id].suspended {
                continue;
            }
            match catch_unwind(AssertUnwindSafe(|| self.modules[idx].suspend())) {
                Ok(Ok(())) => {
                    self.state_view
                        .get_mut(&id)
                        .expect("state exists")
                        .suspended = true;
                }
                _ => log::warn!("module '{id}' failed to suspend; will still finalize on exit"),
            }
        }
    }

    /// 恢复所有挂起模块
    pub fn resume_all(&mut self) {
        let order = self.order.clone();
        for idx in order {
            let id = self.modules[idx].declaration().id.to_string();
            if !self.state_view[&id].online || !self.state_view[&id].suspended {
                continue;
            }
            if catch_unwind(AssertUnwindSafe(|| self.modules[idx].resume())).is_ok() {
                self.state_view
                    .get_mut(&id)
                    .expect("state exists")
                    .suspended = false;
            }
        }
    }

    /// 按初始化逆序收尾。错误只记录，不中断（退出路径必须走完）。
    pub fn finalize_all(&mut self) {
        let mut order = self.order.clone();
        order.reverse();
        for idx in order {
            let id = self.modules[idx].declaration().id.to_string();
            if !self.state_view[&id].online {
                continue;
            }
            if let Err(err) = self.modules[idx].finalize() {
                log::warn!("module '{id}' finalize error: {err}");
            } else {
                log::info!("module '{id}' finalized");
            }
        }
    }

    /// 状态快照（性能仪表盘 / RunReport）
    pub fn states(&self) -> impl Iterator<Item = &ModuleState> {
        self.state_view.values()
    }

    fn mark_offline(&mut self, id: &str, reason: String) {
        log::warn!("module '{id}' offline: {reason}");
        let state = self.state_view.get_mut(id).expect("state exists");
        state.online = false;
        state.note = reason;
    }

    fn mark_crashed(&mut self, id: &str, reason: &str) {
        log::error!("module '{id}' CRASHED, isolating (other modules unaffected): {reason}");
        let state = self.state_view.get_mut(id).expect("state exists");
        state.online = false;
        state.crash_count += 1;
        state.note = format!("crashed: {reason}");
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orange_module_api::{
        DegradeLevel, ModuleDeclaration, ModuleError, ModuleStatus, ResourceBudget,
    };
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeModule {
        id: &'static str,
        deps: Vec<&'static str>,
        init_result: Result<(), ModuleError>,
        events_seen: Mutex<Vec<String>>,
        event_counter: Option<Arc<AtomicU32>>,
        panic_on_event: bool,
        initialized: AtomicBool,
    }

    impl FakeModule {
        fn new(id: &'static str) -> Self {
            Self {
                id,
                deps: Vec::new(),
                init_result: Ok(()),
                events_seen: Mutex::new(Vec::new()),
                event_counter: None,
                panic_on_event: false,
                initialized: AtomicBool::new(false),
            }
        }

        fn with_deps(mut self, deps: Vec<&'static str>) -> Self {
            self.deps = deps;
            self
        }

        fn failing(mut self, err: ModuleError) -> Self {
            self.init_result = Err(err);
            self
        }

        fn counting(mut self, counter: Arc<AtomicU32>) -> Self {
            self.event_counter = Some(counter);
            self
        }

        fn panicking(mut self) -> Self {
            self.panic_on_event = true;
            self
        }
    }

    impl Module for FakeModule {
        fn declaration(&self) -> ModuleDeclaration {
            ModuleDeclaration {
                id: self.id,
                depends_on: self.deps.iter().copied().collect::<BTreeSet<_>>(),
                budget: ResourceBudget {
                    idle_cpu_percent: 0.5,
                    max_memory_mb: 16,
                    needs_gpu: false,
                },
                description: "fake module for tests",
            }
        }

        fn initialize(&mut self, _config: &ModuleConfig) -> Result<(), ModuleError> {
            self.initialized.store(true, Ordering::SeqCst);
            // 测试辅助：init_result 为 Err 时同步返回
            match &self.init_result {
                Err(e) => Err(match e {
                    ModuleError::Unsupported(m) => ModuleError::Unsupported(m.clone()),
                    ModuleError::Crash(m) => ModuleError::Crash(m.clone()),
                    ModuleError::Config(m) => ModuleError::Config(m.clone()),
                    ModuleError::Other(m) => ModuleError::Other(m.clone()),
                }),
                Ok(()) => Ok(()),
            }
        }

        fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
            if self.panic_on_event {
                panic!("boom from {}", self.id);
            }
            if let Some(counter) = &self.event_counter {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            self.events_seen.lock().unwrap().push(format!("{event:?}"));
            Ok(())
        }

        fn suspend(&mut self) -> Result<(), ModuleError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), ModuleError> {
            Ok(())
        }

        fn finalize(&mut self) -> Result<(), ModuleError> {
            Ok(())
        }

        fn status(&self) -> ModuleStatus {
            ModuleStatus {
                level: DegradeLevel::Full,
                detail: String::new(),
            }
        }
    }

    fn boot(registry: &mut ModuleRegistry) {
        registry.resolve().expect("resolve");
        registry.initialize(&BTreeMap::new());
    }

    #[test]
    fn resolves_simple_dependency_order() {
        let mut registry = ModuleRegistry::new();
        // 依赖方向：taskbar -> wallpaper，但注册顺序相反
        registry
            .register(Box::new(
                FakeModule::new("taskbar").with_deps(vec!["wallpaper"]),
            ))
            .unwrap();
        registry
            .register(Box::new(FakeModule::new("wallpaper")))
            .unwrap();
        let order = registry.resolve().unwrap().to_vec();
        let ids: Vec<&'static str> = order.iter().map(|i| registry.modules_get(*i)).collect();
        assert_eq!(ids, ["wallpaper", "taskbar"]);
    }

    #[test]
    fn detects_dependency_cycle() {
        let mut registry = ModuleRegistry::new();
        registry
            .register(Box::new(FakeModule::new("a").with_deps(vec!["b"])))
            .unwrap();
        registry
            .register(Box::new(FakeModule::new("b").with_deps(vec!["a"])))
            .unwrap();
        assert!(matches!(registry.resolve(), Err(RegistryError::Cycle(_))));
    }

    #[test]
    fn rejects_missing_dependency() {
        let mut registry = ModuleRegistry::new();
        registry
            .register(Box::new(FakeModule::new("a").with_deps(vec!["ghost"])))
            .unwrap();
        assert!(matches!(
            registry.resolve(),
            Err(RegistryError::MissingDependency(a, g)) if a == "a" && g == "ghost"
        ));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let mut registry = ModuleRegistry::new();
        registry.register(Box::new(FakeModule::new("dup"))).unwrap();
        assert!(matches!(
            registry.register(Box::new(FakeModule::new("dup"))),
            Err(RegistryError::Duplicate(_))
        ));
    }

    #[test]
    fn init_failure_offlines_only_that_chain() {
        let mut registry = ModuleRegistry::new();
        registry
            .register(Box::new(
                FakeModule::new("wallpaper").failing(ModuleError::Unsupported("no d3d".into())),
            ))
            .unwrap();
        registry
            .register(Box::new(
                FakeModule::new("taskbar").with_deps(vec!["wallpaper"]),
            ))
            .unwrap();
        registry
            .register(Box::new(FakeModule::new("widgets")))
            .unwrap();
        boot(&mut registry);

        let states: Vec<(String, bool)> = registry
            .states()
            .map(|s| (s.id.clone(), s.online))
            .collect();
        assert_eq!(
            states,
            [
                ("taskbar".into(), false),
                ("wallpaper".into(), false),
                ("widgets".into(), true),
            ]
        );
        // 级联 offline 的原因要写明依赖
        let taskbar = registry.states().find(|s| s.id == "taskbar").unwrap();
        assert!(taskbar.note.contains("dependency offline: wallpaper"));
    }

    #[test]
    fn panicking_module_is_isolated_and_others_keep_receiving_events() {
        let good_counter = Arc::new(AtomicU32::new(0));
        let mut registry = ModuleRegistry::new();
        registry
            .register(Box::new(FakeModule::new("bad").panicking()))
            .unwrap();
        registry
            .register(Box::new(
                FakeModule::new("good").counting(good_counter.clone()),
            ))
            .unwrap();
        boot(&mut registry);

        let event = KernelEvent::ShellRestarted;
        registry.dispatch(&event);
        registry.dispatch(&event);

        let bad = registry.states().find(|s| s.id == "bad").unwrap();
        assert!(!bad.online);
        assert_eq!(bad.crash_count, 1);
        assert!(bad.note.contains("panic in on_event"));
        // good 收到两次事件，不受 bad 崩溃影响
        assert_eq!(good_counter.load(Ordering::SeqCst), 2);
    }

    impl ModuleRegistry {
        // 测试辅助：读取模块 id（绕过私有字段可见性）
        fn modules_get(&self, idx: usize) -> &'static str {
            self.modules[idx].declaration().id
        }
    }
}
