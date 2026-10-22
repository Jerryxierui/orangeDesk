//! orangeDesk 桌面小组件模块（§3 模块 B）。
//!
//! 本 crate 落地 v0.3 的跨平台核心：
//! - [`contract::IWidget`] 契约：Metadata（独立更新频率 / 遮挡策略）+ Update + 生命周期
//! - [`scheduler::WidgetScheduler`]：EDF（最早截止优先）统一编排唤醒；
//!   遮挡 / 全局暂停期间不被驱动，恢复时错过的截止一次性到期并立即刷新
//! - 单组件崩溃 → catch_unwind 隔离移除，其他组件无感（§6 v0.3 验收项）
//! - 锁屏 / 电池节能 / 全屏 → 全部组件暂停数据拉取（性能底线，事件驱动）
//!
//! Windows 渲染窗口（D2D 桌面常驻层）与遮挡检测（WindowFromPoint / DWM 事件）
//! 在真机阶段接入；当前数据层已可在线运行（demo / CI 链路验证）。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use std::collections::BTreeSet;

pub mod contract;
pub mod scheduler;
pub mod widgets;

use contract::IWidget;

/// 组件数量上限（§3 B-2：按设备性能动态设定的硬上限；低配设备更低——
/// 动态分级在性能仪表盘数据积累后细化，先固定 16）
pub const MAX_WIDGETS: usize = 16;

/// worker 线程与 Module 之间的控制块
struct HostControls {
    stop: AtomicBool,
    pause: AtomicBool,
}

/// 状态快照（status / 仪表盘数据源）
#[derive(Debug, Default, Clone)]
struct HostShared {
    live_widgets: Vec<String>,
    isolated: Vec<String>,
    ticks: u64,
}

/// 小组件宿主模块
pub struct WidgetHost {
    controls: Arc<HostControls>,
    shared: Arc<Mutex<HostShared>>,
    worker: Option<JoinHandle<()>>,
}

impl WidgetHost {
    pub fn new() -> Self {
        Self {
            controls: Arc::new(HostControls {
                stop: AtomicBool::new(false),
                pause: AtomicBool::new(false),
            }),
            shared: Arc::new(Mutex::new(HostShared::default())),
            worker: None,
        }
    }

    fn set_pause(&self, paused: bool) {
        self.controls.pause.store(paused, Ordering::SeqCst);
    }
}

impl Default for WidgetHost {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 组装第一方组件（B-3 首发集合的前两个；天气 / 媒体控制 / 待办在渲染层后接入）
fn build_first_party_widgets() -> Vec<Box<dyn IWidget>> {
    vec![
        Box::new(widgets::ClockWidget::new()),
        Box::new(widgets::SysMonWidget::new()),
    ]
}

/// 调度线程主循环（拥有全部 widget 与调度器，规避跨线程共享）
fn widget_worker(
    mut widgets: Vec<Box<dyn IWidget>>,
    controls: Arc<HostControls>,
    shared: Arc<Mutex<HostShared>>,
) {
    let mut sched = scheduler::WidgetScheduler::new();
    let start = now_ms();
    let mut sched_ids: Vec<u64> = Vec::with_capacity(widgets.len());
    for widget in &widgets {
        let interval_ms = widget.metadata().update_interval.as_millis() as u64;
        sched_ids.push(sched.insert(interval_ms, start));
    }
    {
        let mut snapshot = shared.lock().unwrap();
        snapshot.live_widgets = widgets
            .iter()
            .map(|w| w.metadata().display_name.to_string())
            .collect();
    }
    log::info!("widgets: {} widget(s) scheduled (EDF)", widgets.len());

    // 首轮立即刷新一次（P2：启动即有内容，不等第一个周期）
    run_due_tick(&mut sched, &mut widgets, &mut sched_ids, &shared);

    let mut last_pause_state = false;
    loop {
        if controls.stop.load(Ordering::SeqCst) {
            break;
        }
        let paused = controls.pause.load(Ordering::SeqCst);
        if paused {
            if !last_pause_state {
                for widget in widgets.iter_mut() {
                    widget.suspend();
                }
                log::info!("widgets: paused (lock screen / power saver / fullscreen)");
            }
            last_pause_state = true;
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if last_pause_state {
            // 恢复：错过的截止已在调度器中积压，下一轮 pop_due 立即全部到期刷新
            for widget in widgets.iter_mut() {
                widget.resume();
            }
            log::info!("widgets: resumed");
            last_pause_state = false;
        }

        let now = now_ms();
        run_due_tick(&mut sched, &mut widgets, &mut sched_ids, &shared);

        // 精确睡到下一个 EDF 截止（时钟 1s 周期 → 每秒仅 1 次唤醒、零空转）；
        // 上限 200ms 保证 stop / pause 标志的检查延迟（「1 秒内生效」纪律内）
        let wake = sched
            .next_deadline_ms()
            .map(|d| d.saturating_sub(now))
            .unwrap_or(200)
            .min(200);
        thread::sleep(Duration::from_millis(wake.max(1)));
    }

    for widget in widgets.iter_mut() {
        widget.finalize();
    }
    log::info!("widgets: worker exited cleanly");
}

/// 执行一轮到期 tick：EDF 顺序更新，单组件崩溃 / Crash 错误 → 隔离移除
fn run_due_tick(
    sched: &mut scheduler::WidgetScheduler,
    widgets: &mut Vec<Box<dyn IWidget>>,
    sched_ids: &mut Vec<u64>,
    shared: &Mutex<HostShared>,
) {
    let now = now_ms();
    for sched_id in sched.pop_due(now) {
        let Some(index) = sched_ids.iter().position(|id| *id == sched_id) else {
            continue;
        };
        let display_name = widgets[index].metadata().display_name.to_string();
        let outcome = catch_unwind(AssertUnwindSafe(|| widgets[index].update(now)));
        match outcome {
            Ok(Ok(())) => {
                if let Ok(mut snapshot) = shared.lock() {
                    snapshot.ticks += 1;
                }
            }
            Ok(Err(err)) => {
                // 组件自报错误：隔离移除（其他组件无感，§6 v0.3 验收）
                log::error!("widgets: '{display_name}' failed, isolating: {err}");
                isolate(sched, widgets, sched_ids, shared, index, &display_name);
            }
            Err(panic) => {
                let reason = panic_message(&panic);
                log::error!("widgets: '{display_name}' panicked, isolating: {reason}");
                isolate(sched, widgets, sched_ids, shared, index, &display_name);
            }
        }
    }
}

fn isolate(
    sched: &mut scheduler::WidgetScheduler,
    widgets: &mut Vec<Box<dyn IWidget>>,
    sched_ids: &mut Vec<u64>,
    shared: &Mutex<HostShared>,
    index: usize,
    display_name: &str,
) {
    let mut removed = widgets.remove(index);
    removed.finalize();
    sched.remove(sched_ids[index]);
    sched_ids.remove(index);
    if let Ok(mut snapshot) = shared.lock() {
        snapshot.live_widgets.retain(|name| name != display_name);
        snapshot.isolated.push(display_name.to_string());
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

impl Module for WidgetHost {
    fn declaration(&self) -> ModuleDeclaration {
        // 预算对齐 §4：小组组件模块（5 个核心组件）< 0.5% CPU
        ModuleDeclaration {
            id: "widgets",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 0.5,
                max_memory_mb: 30,
                needs_gpu: false,
            },
            description: "desktop widgets (IWidget contract, EDF scheduler, occlusion discipline)",
        }
    }

    fn initialize(&mut self, _config: &ModuleConfig) -> Result<(), ModuleError> {
        let widgets = build_first_party_widgets();
        if widgets.len() > MAX_WIDGETS {
            return Err(ModuleError::Config(format!(
                "widget count {} exceeds hard cap {MAX_WIDGETS}",
                widgets.len()
            )));
        }
        let controls = self.controls.clone();
        let shared = self.shared.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("od-widget-worker".to_string())
                .spawn(move || widget_worker(widgets, controls, shared))
                .map_err(|err| ModuleError::Other(format!("spawn widget worker: {err}")))?,
        );
        Ok(())
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            // 全局暂停：锁屏 / 电池节能 / 全屏（性能底线：数据拉取也停）
            KernelEvent::LockScreenChanged { locked: true }
            | KernelEvent::PowerSaverChanged { enabled: true }
            | KernelEvent::FullscreenChanged { fullscreen: true } => self.set_pause(true),
            KernelEvent::LockScreenChanged { locked: false }
            | KernelEvent::PowerSaverChanged { enabled: false }
            | KernelEvent::FullscreenChanged { fullscreen: false } => self.set_pause(false),
            _ => {}
        }
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), ModuleError> {
        self.set_pause(true);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ModuleError> {
        self.set_pause(false);
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), ModuleError> {
        self.controls.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn status(&self) -> ModuleStatus {
        let snapshot = self.shared.lock().map(|s| s.clone()).unwrap_or_default();
        let detail = format!(
            "{} live, {} tick(s){}",
            snapshot.live_widgets.len(),
            snapshot.ticks,
            if snapshot.isolated.is_empty() {
                String::new()
            } else {
                format!(", isolated: {}", snapshot.isolated.join(", "))
            }
        );
        ModuleStatus {
            level: DegradeLevel::Full,
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{OcclusionPolicy, WidgetError, WidgetFrame, WidgetMetadata};

    struct CountdownWidget {
        remaining: u32,
    }

    impl IWidget for CountdownWidget {
        fn metadata(&self) -> WidgetMetadata {
            WidgetMetadata {
                id: "countdown",
                display_name: "Countdown",
                update_interval: Duration::from_millis(10),
                occlusion_policy: OcclusionPolicy::Pause,
            }
        }
        fn update(&mut self, _now_ms: u64) -> Result<(), WidgetError> {
            self.remaining -= 1;
            if self.remaining == 0 {
                return Err(WidgetError::Crash("countdown reached zero".into()));
            }
            Ok(())
        }
        fn suspend(&mut self) {}
        fn resume(&mut self) {}
        fn finalize(&mut self) {}
        fn frame(&self) -> WidgetFrame {
            WidgetFrame::default()
        }
    }

    struct FlakyPanicWidget;

    impl IWidget for FlakyPanicWidget {
        fn metadata(&self) -> WidgetMetadata {
            WidgetMetadata {
                id: "panic",
                display_name: "Panic",
                update_interval: Duration::from_millis(10),
                occlusion_policy: OcclusionPolicy::Pause,
            }
        }
        fn update(&mut self, _now_ms: u64) -> Result<(), WidgetError> {
            panic!("boom");
        }
        fn suspend(&mut self) {}
        fn resume(&mut self) {}
        fn finalize(&mut self) {}
        fn frame(&self) -> WidgetFrame {
            WidgetFrame::default()
        }
    }

    #[test]
    fn worker_isolates_crashed_widget_and_keeps_others_running() {
        let controls = Arc::new(HostControls {
            stop: AtomicBool::new(false),
            pause: AtomicBool::new(false),
        });
        let shared = Arc::new(Mutex::new(HostShared::default()));
        let widgets: Vec<Box<dyn IWidget>> = vec![
            Box::new(CountdownWidget { remaining: 2 }),
            Box::new(widgets::ClockWidget::new()),
            Box::new(FlakyPanicWidget),
        ];

        let ctrl = controls.clone();
        let snap = shared.clone();
        let handle = thread::spawn(move || widget_worker(widgets, ctrl, snap));
        // 让 worker 跑足够多轮：countdown 2 轮后 Crash、panic 立刻炸
        thread::sleep(Duration::from_millis(400));
        controls.stop.store(true, Ordering::SeqCst);
        let _ = handle.join();

        let snapshot = shared.lock().unwrap().clone();
        assert_eq!(snapshot.isolated.len(), 2, "both bad widgets isolated");
        assert!(
            snapshot.live_widgets.contains(&"Clock".to_string()),
            "healthy widget survived"
        );
        assert!(snapshot.ticks > 0);
    }

    #[test]
    fn pause_and_resume_flow_through_controls() {
        let host = WidgetHost::new();
        host.set_pause(true);
        assert!(host.controls.pause.load(Ordering::SeqCst));
        host.set_pause(false);
        assert!(!host.controls.pause.load(Ordering::SeqCst));
    }

    #[test]
    fn first_party_widget_count_within_cap() {
        assert!(build_first_party_widgets().len() <= MAX_WIDGETS);
    }
}
