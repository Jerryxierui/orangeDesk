//! 壁纸渲染器：D3D11 flip-model 交换链 + DirectComposition 提交（§3 A-1）。
//!
//! 管线纪律（从第一天就按 DComp 提交模型设计，GDI/blt 永不作主路径）：
//! - `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` 双缓冲 + `Present(SyncInterval=1)` VSync 对齐
//! - flip-model 后缓冲不接受 `UpdateSubresource` / `CopyResource` CPU 写入
//!   （未来视频帧必须走 GPU video-processor blit，禁止 CPU 回读）
//! - 暂停而非降帧（性能底线）：pause 后不再 Present，并调用
//!   `IDXGIDevice3::Trim` 主动归还显存（不依赖系统被动回收）
//! - 静态内容只绘制一次（脏标记驱动）；视频 / 图片模式接入后按档位持续提交
//!
//! 线程模型：窗口（挂载锚点）与 DComp target 必须同线程，
//! 因此 mount + 设备创建 + 帧循环全部在渲染线程内完成。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::topology::DesktopTopology;

/// 渲染配置
#[derive(Debug, Clone, Copy)]
pub struct RenderConfig {
    /// Clear 颜色（sRGB 分量 0.0–1.0）
    pub color: [f32; 4],
    /// 目标帧间隔（ms）；0 = 仅 VSync 对齐
    pub frame_interval_ms: u64,
}

/// 线程间控制块
struct Controls {
    run: AtomicBool,
    pause: AtomicBool,
    /// 暂停期间是否已执行过 Trim（避免每 100ms 重复 Trim）
    trimmed: AtomicBool,
}

/// 渲染器句柄：持有渲染线程，提供暂停 / 恢复 / 停止控制
pub struct Renderer {
    controls: Arc<Controls>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    /// 启动渲染线程（内部完成拓扑挂载 + D3D/DComp 初始化 + 帧循环）
    pub fn spawn(topology: DesktopTopology, config: RenderConfig) -> Result<Self, String> {
        let controls = Arc::new(Controls {
            run: AtomicBool::new(true),
            pause: AtomicBool::new(false),
            trimmed: AtomicBool::new(false),
        });
        let thread_controls = controls.clone();
        let thread = thread::Builder::new()
            .name("od-wallpaper-render".to_string())
            .spawn(move || {
                run_render_loop(topology, config, &thread_controls);
            })
            .map_err(|err| format!("spawn render thread: {err}"))?;
        Ok(Self {
            controls,
            thread: Some(thread),
        })
    }

    /// 暂停渲染并触发显存 Trim（幂等；重复调用无额外开销）
    pub fn pause_and_trim(&self) {
        self.controls.pause.store(true, Ordering::SeqCst);
    }

    /// 恢复渲染
    pub fn unpause(&self) {
        self.controls.pause.store(false, Ordering::SeqCst);
        self.controls.trimmed.store(false, Ordering::SeqCst);
    }

    /// 停止渲染线程并回收资源（幂等）
    pub fn stop(&mut self) -> Result<(), String> {
        self.controls.run.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .map_err(|_| "wallpaper render thread panicked".to_string())?;
        }
        Ok(())
    }
}

/// 渲染线程主循环
fn run_render_loop(topology: DesktopTopology, config: RenderConfig, controls: &Controls) {
    unsafe {
        // 1. 拓扑挂载（必须在渲染线程：窗口线程相关性）
        let Ok(host) = crate::mount::create_wallpaper_child(&topology) else {
            log::error!("wallpaper: mount failed: {:?}", topology);
            return;
        };

        // 2. D3D11 设备（硬件 + BGRA；视频路径后续追加 VIDEO_SUPPORT）
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_SDK_VERSION,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIDevice3, IDXGIFactory2, DXGI_SCALING_STRETCH,
            DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            DXGI_USAGE_RENDER_TARGET_OUTPUT,
        };

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        if let Err(err) = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        ) {
            log::error!("wallpaper: D3D11CreateDevice failed: {err}");
            return;
        }
        let device = device.expect("device out-param on success");
        let context = context.expect("context out-param on success");

        // 3. DXGI 设备 / 工厂 + composition 交换链（flip-model，DComp 定尺寸）
        use windows::core::Interface;
        let dxgi_device = match device.cast::<IDXGIDevice3>() {
            Ok(d) => d,
            Err(err) => {
                log::error!("wallpaper: cast IDXGIDevice3 failed: {err}");
                return;
            }
        };
        let factory: IDXGIFactory2 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(err) => {
                log::error!("wallpaper: CreateDXGIFactory1 failed: {err}");
                return;
            }
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: windows::core::BOOL::default(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swapchain = match factory.CreateSwapChainForComposition(&device, &desc, None) {
            Ok(s) => s,
            Err(err) => {
                log::error!("wallpaper: CreateSwapChainForComposition failed: {err}");
                return;
            }
        };

        // 4. DirectComposition 树：visual 内容 = 交换链，挂到壁纸宿主窗口
        use windows::Win32::Graphics::DirectComposition::{
            DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
        };
        let dcomp_device: IDCompositionDevice =
            match DCompositionCreateDevice::<_, IDCompositionDevice>(&dxgi_device) {
                Ok(d) => d,
                Err(err) => {
                    log::error!("wallpaper: DCompositionCreateDevice failed: {err}");
                    return;
                }
            };
        let target: IDCompositionTarget = match dcomp_device.CreateTargetForHwnd(host, true) {
            Ok(t) => t,
            Err(err) => {
                log::error!("wallpaper: CreateTargetForHwnd failed: {err}");
                return;
            }
        };
        let visual: IDCompositionVisual = match dcomp_device.CreateVisual() {
            Ok(v) => v,
            Err(err) => {
                log::error!("wallpaper: CreateVisual failed: {err}");
                return;
            }
        };
        // SetContent 接受 IUnknown 参数（COM 层级），显式上转
        let swapchain_unknown = match swapchain.cast::<windows::core::IUnknown>() {
            Ok(u) => u,
            Err(err) => {
                log::error!("wallpaper: swapchain cast IUnknown failed: {err}");
                return;
            }
        };
        if let Err(err) = visual.SetContent(Some(&swapchain_unknown)) {
            log::error!("wallpaper: SetContent failed: {err}");
            return;
        }
        if let Err(err) = target.SetRoot(Some(&visual)) {
            log::error!("wallpaper: SetRoot failed: {err}");
            return;
        }
        if let Err(err) = dcomp_device.Commit() {
            log::error!("wallpaper: DComp Commit failed: {err}");
            return;
        }
        log::info!("wallpaper: DComp pipeline ready (flip-model, 2 buffers)");

        // 5. 首帧（静态内容脏标记驱动：绘制一次即可）
        if !draw_frame(&device, &context, &swapchain, config.color) {
            return;
        }

        // 6. 帧循环：消息泵 + 控制标志。无高频轮询（P4），
        //    50ms 心跳只为及时响应暂停 / 停机。
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
        };
        let mut msg = MSG::default();
        loop {
            if !controls.run.load(Ordering::SeqCst) {
                break;
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }

            if controls.pause.load(Ordering::SeqCst) {
                if !controls.trimmed.swap(true, Ordering::SeqCst) {
                    // 暂停即让出 GPU：主动 Trim 显存（A-1 纪律）
                    dxgi_device.Trim();
                    log::info!("wallpaper: paused, GPU memory trimmed");
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            // TODO(v0.1): 视频 / 图片模式在此按 frame_interval_ms 持续提交帧；
            // 纯色模式无脏数据，跳过 Present 以保持空闲占用接近 0。
            thread::sleep(Duration::from_millis(50));
        }

        // 7. 清理：断开 visual 树并提交，释放 DComp target
        let _ = target.SetRoot(None);
        let _ = dcomp_device.Commit();
        log::info!("wallpaper: render thread exited cleanly");
    }
}

/// 绘制一帧到后缓冲并 VSync 提交
unsafe fn draw_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    swapchain: &windows::Win32::Graphics::Dxgi::IDXGISwapChain1,
    color: [f32; 4],
) -> bool {
    use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;
    unsafe {
        let back_buffer: ID3D11Texture2D = match swapchain.GetBuffer(0) {
            Ok(b) => b,
            Err(err) => {
                log::error!("wallpaper: GetBuffer failed: {err}");
                return false;
            }
        };
        // CreateRenderTargetView 为出参形式
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        if let Err(err) = device.CreateRenderTargetView(&back_buffer, None, Some(&mut rtv)) {
            log::error!("wallpaper: CreateRenderTargetView failed: {err}");
            return false;
        }
        let Some(rtv) = rtv else {
            log::error!("wallpaper: CreateRenderTargetView returned no view");
            return false;
        };
        context.ClearRenderTargetView(&rtv, &color);
        // Present 返回裸 HRESULT：DXGI_ERROR 设备移除等场景需记录
        let hr = swapchain.Present(1, DXGI_PRESENT(0));
        if hr.is_err() {
            log::error!("wallpaper: Present failed: {hr:?}");
            return false;
        }
        true
    }
}
