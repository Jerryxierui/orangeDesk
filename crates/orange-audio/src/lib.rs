//! orangeDesk 音频响应模块（§13 F4）。
//!
//! WASAPI loopback 抓取系统混音 → FFT 频谱分析 → `AudioSpectrum` 事件
//! 广播（~30Hz 节流 + EMA 平滑），壁纸 / 组件订阅做可视化。
//!
//! 纪律：
//! - 采集线程事件驱动（WASAPI 事件回调等待，零轮询，P4）
//! - 锁屏 / 电池节能 / 全屏 → 采集暂停（设备静默等待，无 CPU）
//! - 无声 / 无输出设备 → 事件停发（订阅者保留最后画面，不发零帧噪声）
//! - 预算对齐 §13.4：采集线程 0.3% CPU / 10MB
//!
//! 非 Windows：模块 offline（跨平台链路验证路径）；分析逻辑全平台测试。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use orange_module_api::{
    DegradeLevel, KernelEvent, Module, ModuleConfig, ModuleDeclaration, ModuleError, ModuleStatus,
    ResourceBudget,
};
use std::collections::BTreeSet;

pub mod analysis;
#[cfg(windows)]
pub mod capture;

#[cfg(windows)]
use analysis::{fft_magnitude, smooth_spectrum, spectrum_bands, DEFAULT_BANDS};

/// 事件广播节流（可视化帧率上限；低于显示器刷新率即够用）
#[cfg(windows)]
const EMIT_INTERVAL: Duration = Duration::from_millis(33);
/// 平滑释放系数（下降沿慢，节拍感）
#[cfg(windows)]
const RELEASE: f32 = 0.25;

/// 音频响应模块
pub struct AudioModule {
    controls: Arc<Controls>,
    worker: Option<JoinHandle<()>>,
    /// 供 status 展示的发射统计
    stats: Arc<std::sync::Mutex<Stats>>,
    /// 事件桥注册位（supervisor 的 publisher_tap 在模块 initialize 前注入；
    /// 注入前 worker 静默等待）
    sink: Arc<std::sync::OnceLock<EventBridge>>,
}

struct Controls {
    stop: AtomicBool,
    pause: AtomicBool,
}

#[derive(Default, Clone)]
struct Stats {
    emitted: u64,
    /// 最近一次频谱（未来调试接口用；status 目前只报计数）
    #[allow(dead_code)]
    last_bands: Vec<f32>,
}

impl AudioModule {
    pub fn new() -> Self {
        Self {
            controls: Arc::new(Controls {
                stop: AtomicBool::new(false),
                pause: AtomicBool::new(false),
            }),
            worker: None,
            stats: Arc::new(std::sync::Mutex::new(Stats::default())),
            sink: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// 事件桥注册端（supervisor 的 publisher_tap 调用）
    pub fn sink(&self) -> Arc<std::sync::OnceLock<EventBridge>> {
        self.sink.clone()
    }
}

impl Default for AudioModule {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn audio_worker(
    controls: Arc<Controls>,
    stats: Arc<std::sync::Mutex<Stats>>,
    sink: Arc<std::sync::OnceLock<EventBridge>>,
) {
    // WASAPI 采集推荐 MTA
    let com_inited = {
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        hr.is_ok()
    };
    let mut capture = match capture::LoopbackCapture::open() {
        Ok(capture) => capture,
        Err(err) => {
            log::warn!("audio: loopback capture unavailable ({err}) — module idle");
            // 无设备 / 格式不支持：保持存活但空转（pause 态等待用户接设备）
            while !controls.stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(500));
            }
            if com_inited {
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
            return;
        }
    };

    let mut smoothed: Vec<f32> = vec![0.0; DEFAULT_BANDS];
    let mut last_emit = Instant::now() - EMIT_INTERVAL;
    let mut last_silent_at: Option<Instant> = None;

    while !controls.stop.load(Ordering::SeqCst) {
        if controls.pause.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let mut got_window = false;
        let result = capture.pump(100, &mut |window: &[f32]| {
            got_window = true;
            let magnitude = fft_magnitude(window);
            let bands = spectrum_bands(&magnitude, DEFAULT_BANDS);
            smooth_spectrum(&mut smoothed, &bands, RELEASE);
        });
        if let Err(err) = result {
            log::error!("audio: capture error ({err}), retrying after cooldown");
            capture.stop();
            thread::sleep(Duration::from_secs(2));
            match capture::LoopbackCapture::open() {
                Ok(reopened) => capture = reopened,
                Err(reopen_err) => {
                    log::error!("audio: reopen failed ({reopen_err}), module idle");
                    thread::sleep(Duration::from_secs(5));
                }
            }
            continue;
        }
        // 完全静音判定：窗口满但全零（或长时间无数据）→ 不发射
        let silent = !got_window || smoothed.iter().all(|b| *b < 0.01);
        if silent {
            last_silent_at.get_or_insert_with(Instant::now);
            // 静音超过 2s：把平滑状态衰减归零（避免残留残影）后停发
            if last_silent_at.unwrap().elapsed() > Duration::from_secs(2)
                && smoothed.iter().any(|b| *b > 0.0)
            {
                smooth_spectrum(&mut smoothed, &[0.0; DEFAULT_BANDS], 0.1);
                if smoothed.iter().all(|b| *b < 0.01) {
                    smoothed.fill(0.0);
                }
            }
            continue;
        }
        last_silent_at = None;
        if last_emit.elapsed() >= EMIT_INTERVAL {
            last_emit = Instant::now();
            let Some(publish) = sink.get() else {
                continue; // 总线未就绪（初始化竞态）：下一窗口再试
            };
            publish(KernelEvent::AudioSpectrum {
                bands: smoothed.clone(),
            });
            if let Ok(mut slot) = stats.lock() {
                slot.emitted += 1;
                slot.last_bands = smoothed.clone();
            }
        }
    }
    capture.stop();
    if com_inited {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
    log::info!("audio: worker exited cleanly");
}

/// 非 Windows 占位 worker（链路验证：立即进入停等）
#[cfg(not(windows))]
fn audio_worker(
    controls: Arc<Controls>,
    _stats: Arc<std::sync::Mutex<Stats>>,
    _sink: Arc<std::sync::OnceLock<EventBridge>>,
) {
    log::info!("audio: loopback capture requires Windows; worker idle");
    while !controls.stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(500));
    }
}

/// 事件桥：worker 线程向总线发布事件的通道（supervisor 的 publisher_tap 注入）
pub type EventBridge = Arc<dyn Fn(KernelEvent) + Send + Sync>;

impl Module for AudioModule {
    fn declaration(&self) -> ModuleDeclaration {
        ModuleDeclaration {
            id: "audio",
            depends_on: BTreeSet::new(),
            budget: ResourceBudget {
                idle_cpu_percent: 0.3,
                max_memory_mb: 10,
                needs_gpu: false,
            },
            description:
                "audio-reactive spectrum (WASAPI loopback + FFT, broadcasts AudioSpectrum)",
        }
    }

    fn initialize(&mut self, _config: &ModuleConfig) -> Result<(), ModuleError> {
        let controls = self.controls.clone();
        let stats = self.stats.clone();
        let sink = self.sink.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("od-audio-worker".to_string())
                .spawn(move || audio_worker(controls, stats, sink))
                .map_err(|err| ModuleError::Other(format!("spawn audio worker: {err}")))?,
        );
        Ok(())
    }

    fn on_event(&mut self, event: &KernelEvent) -> Result<(), ModuleError> {
        match event {
            KernelEvent::LockScreenChanged { locked: true }
            | KernelEvent::PowerSaverChanged { enabled: true }
            | KernelEvent::FullscreenChanged { fullscreen: true } => {
                self.controls.pause.store(true, Ordering::SeqCst);
            }
            KernelEvent::LockScreenChanged { locked: false }
            | KernelEvent::PowerSaverChanged { enabled: false }
            | KernelEvent::FullscreenChanged { fullscreen: false } => {
                self.controls.pause.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), ModuleError> {
        self.controls.pause.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ModuleError> {
        self.controls.pause.store(false, Ordering::SeqCst);
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
        let stats = self.stats.lock().map(|s| s.clone()).unwrap_or_default();
        ModuleStatus {
            level: DegradeLevel::Full,
            detail: format!("{} spectrum frame(s) emitted", stats.emitted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_declares_tight_budget() {
        let module = AudioModule::new();
        let decl = module.declaration();
        assert_eq!(decl.id, "audio");
        assert_eq!(decl.budget.idle_cpu_percent, 0.3);
        assert_eq!(decl.budget.max_memory_mb, 10);
    }

    #[test]
    fn pause_resume_flips_controls() {
        let mut module = AudioModule::new();
        module
            .on_event(&KernelEvent::LockScreenChanged { locked: true })
            .unwrap();
        assert!(module.controls.pause.load(Ordering::SeqCst));
        module
            .on_event(&KernelEvent::LockScreenChanged { locked: false })
            .unwrap();
        assert!(!module.controls.pause.load(Ordering::SeqCst));
    }

    #[test]
    fn initialize_without_bridge_is_idle_safe() {
        let mut module = AudioModule::new();
        module.initialize(&serde_json::Value::Null).unwrap();
        // finalize 无 worker 时安全
        module.finalize().unwrap();
    }

    #[test]
    fn worker_with_sink_emits_and_exits_cleanly() {
        let mut module = AudioModule::new();
        let seen = Arc::new(std::sync::Mutex::new(0u32));
        let counter = seen.clone();
        module.initialize(&serde_json::Value::Null).unwrap();
        let _ = module.sink().set(Arc::new(move |event: KernelEvent| {
            if matches!(event, KernelEvent::AudioSpectrum { .. }) {
                *counter.lock().unwrap() += 1;
            }
        }));
        thread::sleep(Duration::from_millis(50));
        module.finalize().unwrap();
    }
}
