//! 壁纸渲染器：D3D11 flip-model 交换链 + DirectComposition 提交（§3 A-1）。
//!
//! 管线纪律（从第一天就按 DComp 提交模型设计，GDI/blt 永不作主路径）：
//! - `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` 双缓冲 + `Present(SyncInterval=1)` VSync 对齐
//! - flip-model 后缓冲不接受 `UpdateSubresource` / `CopyResource` CPU 写入
//!   （未来视频帧必须走 GPU video-processor blit，禁止 CPU 回读）
//! - CPU 侧内容（图片）按文档 A-1 走 Direct2D `DrawBitmap` 提交（GPU 直绘
//!   到交换链后缓冲，无 CPU 回读、无 CPU 拷贝到后缓冲）
//! - 暂停而非降帧（性能底线）：pause 后不再 Present，并调用
//!   `IDXGIDevice3::Trim` 主动归还显存（不依赖系统被动回收）
//! - 静态内容只绘制一次（脏标记驱动）；视频模式接入后按档位持续提交
//!
//! 线程模型：窗口（挂载锚点）与 DComp target 必须同线程，
//! 因此 mount + 设备创建 + 帧循环全部在渲染线程内完成。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::geometry::{cover_rect, Size};
use crate::topology::DesktopTopology;

/// 首帧内容源
#[derive(Debug, Clone)]
pub enum ContentSource {
    /// 纯色填充（默认模式 / 图片失败降级）
    Solid { color: [f32; 4] },
    /// 静态图片（WIC 解码 + D2D DrawBitmap，cover 等比缩放）；
    /// 解码失败时用 `fallback_color` 填充并记录明确原因（P3 不静默失败）
    Image {
        path: String,
        fallback_color: [f32; 4],
    },
    /// 视频（MF 硬解 + GPU VideoProcessor blit，循环播放，静音）；
    /// 管线失败时用 `fallback_color` 填充并记录明确原因
    Video {
        path: String,
        fallback_color: [f32; 4],
    },
}

impl ContentSource {
    /// 所有模式统一的降级颜色
    fn fallback_color(&self) -> [f32; 4] {
        match self {
            ContentSource::Solid { color }
            | ContentSource::Image {
                fallback_color: color,
                ..
            }
            | ContentSource::Video {
                fallback_color: color,
                ..
            } => *color,
        }
    }
}

/// 渲染配置
#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub content: ContentSource,
    /// 目标帧间隔（ms）；0 = 仅 VSync 对齐（视频模式使用，静态内容无关）
    pub frame_interval_ms: u64,
}

/// composition 路径的 DComp 树句柄（清理时断开 visual 用）
struct DCompTree {
    device: windows::Win32::Graphics::DirectComposition::IDCompositionDevice,
    target: windows::Win32::Graphics::DirectComposition::IDCompositionTarget,
}

/// 线程间控制块
struct Controls {
    run: AtomicBool,
    pause: AtomicBool,
    /// 暂停期间是否已执行过 Trim（避免每 100ms 重复 Trim）
    trimmed: AtomicBool,
    /// 渲染线程的终止原因（None = 运行中 / 正常退出）；模块 status 读取（P6）
    error: std::sync::Mutex<Option<String>>,
    /// 渲染线程 ID（0 = 尚未记录）；控制方法 PostThreadMessage 即时唤醒用
    thread_id: std::sync::atomic::AtomicU32,
    /// 当前帧率档位间隔（ms）；§5 降级矩阵经 set_frame_interval 下调
    frame_interval_ms: std::sync::atomic::AtomicU64,
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
            error: std::sync::Mutex::new(None),
            thread_id: std::sync::atomic::AtomicU32::new(0),
            frame_interval_ms: std::sync::atomic::AtomicU64::new(config.frame_interval_ms.max(1)),
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

    /// 用线程消息唤醒阻塞在 MsgWaitForMultipleObjectsEx 的渲染线程
    /// （线程已退出时 PostThreadMessage 失败，安全忽略）
    fn wake(&self) {
        let tid = self.controls.thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_NULL};
                let _ = PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// 暂停渲染并触发显存 Trim（幂等；重复调用无额外开销）
    pub fn pause_and_trim(&self) {
        self.controls.pause.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// 恢复渲染
    pub fn unpause(&self) {
        self.controls.pause.store(false, Ordering::SeqCst);
        self.controls.trimmed.store(false, Ordering::SeqCst);
        self.wake();
    }

    /// 停止渲染线程并回收资源（幂等）
    pub fn stop(&mut self) -> Result<(), String> {
        self.controls.run.store(false, Ordering::SeqCst);
        self.wake();
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .map_err(|_| "wallpaper render thread panicked".to_string())?;
        }
        Ok(())
    }

    /// 渲染线程终止原因（None = 运行中或正常退出）；status / 仪表盘展示用（P6）
    pub fn render_error(&self) -> Option<String> {
        self.controls.error.lock().ok().and_then(|e| e.clone())
    }

    /// 运行时调整帧率档位（§5 降级矩阵：预算压力下调）；唤醒线程立即生效
    pub fn set_frame_interval(&self, frame_interval_ms: u64) {
        self.controls
            .frame_interval_ms
            .store(frame_interval_ms.max(1), Ordering::SeqCst);
        self.wake();
    }
}

/// 渲染线程主循环（负责 COM 生命周期包裹）
fn run_render_loop(topology: DesktopTopology, config: RenderConfig, controls: &Controls) {
    // 记录线程 ID：控制方法（stop/pause/unpause）PostThreadMessage 即时唤醒本线程
    controls.thread_id.store(
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        Ordering::SeqCst,
    );

    // WIC 是 COM 组件：渲染线程 STA 初始化（S_OK / S_FALSE 均视为已初始化）
    #[cfg(windows)]
    let com_inited = {
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        hr.is_ok()
    };

    let result = run_render_loop_inner(topology, &config, controls);

    #[cfg(windows)]
    if com_inited {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }

    if let Err(err) = result {
        log::error!("wallpaper render loop: {err}");
        // 终止原因上报给模块 status（P6：渲染失败必须用户可见，不静默）
        if let Ok(mut slot) = controls.error.lock() {
            *slot = Some(err);
        }
    }
}

fn run_render_loop_inner(
    topology: DesktopTopology,
    config: &RenderConfig,
    controls: &Controls,
) -> Result<(), String> {
    unsafe {
        // 1. 拓扑挂载（必须在渲染线程：窗口线程相关性）
        let host = crate::mount::create_wallpaper_child(&topology)?;

        // 2. D3D11 设备：硬件优先，失败回退 WARP 软件光栅
        //    （无 GPU / 无头会话如 CI runner；A-1 降级序列的起点）
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_SDK_VERSION,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIDevice3, DXGI_PRESENT, DXGI_SCALING_STRETCH,
            DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            DXGI_USAGE_RENDER_TARGET_OUTPUT,
        };

        let create_device = |driver_type| {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                driver_type,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .ok()
            .and_then(|_| device.zip(context))
        };
        let (device, context, using_warp) = match create_device(D3D_DRIVER_TYPE_HARDWARE) {
            Some(pair) => (pair.0, pair.1, false),
            None => {
                let (device, context) = create_device(D3D_DRIVER_TYPE_WARP)
                    .ok_or("D3D11CreateDevice failed (hardware and WARP)")?;
                log::warn!("wallpaper: hardware D3D unavailable, using WARP software rasterizer");
                (device, context, true)
            }
        };

        // 3. DXGI 设备 / 工厂 + 交换链（flip-model）。
        //    A-1 降级序列：DirectComposition 提交（首选）→ hwnd 关联交换链
        //    （Classic WorkerW 时代的常规管线，无需 DComp）。
        use windows::core::Interface;
        let dxgi_device = device
            .cast::<IDXGIDevice3>()
            .map_err(|err| format!("cast IDXGIDevice3 failed: {err}"))?;
        let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory2 =
            CreateDXGIFactory1().map_err(|err| format!("CreateDXGIFactory1 failed: {err}"))?;
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
        use windows::Win32::Graphics::DirectComposition::{
            DCompositionCreateDevice, IDCompositionDevice,
        };
        // 主路径：composition flip-model（交换链 + DComp 树一并就绪才算成功）
        let composition_path = |factory: &windows::Win32::Graphics::Dxgi::IDXGIFactory2| {
            let swapchain = factory.CreateSwapChainForComposition(&device, &desc, None)?;
            let dcomp_device: IDCompositionDevice =
                DCompositionCreateDevice::<_, IDCompositionDevice>(&dxgi_device)?;
            let target = dcomp_device.CreateTargetForHwnd(host, true)?;
            let visual = dcomp_device.CreateVisual()?;
            // SetContent 接受 IUnknown 参数（COM 层级），显式上转
            let swapchain_unknown = swapchain.cast::<windows::core::IUnknown>()?;
            visual.SetContent(Some(&swapchain_unknown))?;
            target.SetRoot(Some(&visual))?;
            dcomp_device.Commit()?;
            Ok::<_, windows::core::Error>((
                swapchain,
                Some(DCompTree {
                    device: dcomp_device,
                    target,
                }),
            ))
        };
        let (swapchain, dcomp) = match composition_path(&factory) {
            Ok(pair) => {
                log::info!("wallpaper: DComp pipeline ready (flip-model, 2 buffers)");
                pair
            }
            Err(err) => {
                // 降级：hwnd 关联交换链（无 DComp；WARP 场景 / DComp 受限环境）
                log::warn!(
                    "wallpaper: composition path unavailable ({err}), falling back to hwnd swapchain"
                );
                let swapchain = factory
                    .CreateSwapChainForHwnd(&device, host, &desc, None, None)
                    .map_err(|err| format!("CreateSwapChainForHwnd fallback failed: {err}"))?;
                log::info!("wallpaper: hwnd flip-model pipeline ready (no DComp)");
                (swapchain, None)
            }
        };
        if using_warp {
            log::warn!("wallpaper: running on WARP — expect limited performance (headless / no-GPU environment?)");
        }

        // 5. 首帧：按内容源分发。静态内容（纯色 / 图片）脏标记驱动、绘制一次；
        //    视频模式建立解码管线，首帧由第 6 步循环按帧率档位立即提交。
        let (screen_w, screen_h) = host_client_size(host);
        let fallback_color = config.content.fallback_color();
        let mut video: Option<crate::video::VideoPipeline> = None;
        let mut mf_inited = false;
        match &config.content {
            ContentSource::Solid { color } => {
                draw_solid_frame(&device, &context, &swapchain, *color)?;
            }
            ContentSource::Image {
                path,
                fallback_color,
            } => {
                let drawn = draw_image_frame(&dxgi_device, &swapchain, path, screen_w, screen_h)
                    .unwrap_or_else(|err| {
                        log::warn!("wallpaper: image pipeline error: {err}");
                        false
                    });
                if drawn {
                    log::info!("wallpaper: image content drawn ({path})");
                } else {
                    // 降级纯色：明确记录，不静默黑屏（P3/P6）
                    log::warn!("wallpaper: image degraded to solid fill ({path})");
                    draw_solid_frame(&device, &context, &swapchain, *fallback_color)?;
                }
            }
            ContentSource::Video { path, .. } => {
                mf_inited = true;
                match crate::video::VideoPipeline::new(
                    &device,
                    &context,
                    path,
                    screen_w as u32,
                    screen_h as u32,
                ) {
                    Ok(pipeline) => {
                        log::info!("wallpaper: video pipeline ready ({path})");
                        video = Some(pipeline);
                    }
                    Err(err) => {
                        log::warn!("wallpaper: video degraded to solid fill ({path}): {err}");
                        draw_solid_frame(&device, &context, &swapchain, fallback_color)?;
                    }
                }
            }
        }

        // 6. 帧循环：消息泵 + 控制标志。等待用 MsgWaitForMultipleObjectsEx
        //    （有消息或超时才醒）：静态内容空闲时线程纯睡眠、零轮询唤醒（P4）；
        //    stop / pause 由控制方法 PostThreadMessage 即时唤醒；
        //    视频模式由 FramePacer 按档位（30/60/120/Unlimited）驱动
        //    「解码一帧 → VP blt → Present(VSync)」。
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
        };
        let mut pacer = crate::pacing::FramePacer::new(config.frame_interval_ms);
        let mut last_restart: Option<std::time::Instant> = None;
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
                // 等消息（unpause 的 PostThreadMessage 即时唤醒；超时兜底）
                wait_for_message(Duration::from_millis(100));
                continue;
            }

            if let Some(pipeline) = video.as_mut() {
                // §5 降级矩阵：档位可被 set_frame_interval 运行时下调，逐轮同步
                pacer.set_interval(controls.frame_interval_ms.load(Ordering::SeqCst));
                let now = std::time::Instant::now();
                if !pacer.should_present(now) {
                    // 睡到下一帧截止（等消息可提前醒：停机 / 暂停即时生效）
                    wait_for_message(pacer.until_next(now).min(Duration::from_millis(250)));
                    continue;
                }
                match pipeline.blt_next_frame(&swapchain) {
                    Ok(true) => {
                        let hr = swapchain.Present(1, DXGI_PRESENT(0));
                        if hr.is_err() {
                            log::error!("wallpaper: video Present failed: {hr:?}");
                            video = None;
                        } else {
                            pacer.mark_presented(std::time::Instant::now());
                        }
                    }
                    Ok(false) => {
                        // EOF：循环重播（重建 SourceReader）。
                        // 极短视频（亚秒级）EOF 会高频触发——重建节流 ≥1s 一次，
                        // 防止每帧 open+init 解码器的 CPU 空转
                        let restart_due =
                            last_restart.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
                        if restart_due {
                            match pipeline.restart(&device) {
                                Ok(()) => last_restart = Some(std::time::Instant::now()),
                                Err(err) => {
                                    log::error!("wallpaper: video loop restart failed: {err}");
                                    video = None;
                                }
                            }
                        } else {
                            wait_for_message(Duration::from_millis(100));
                        }
                    }
                    Err(err) => {
                        log::error!("wallpaper: video pipeline failed: {err}");
                        video = None;
                    }
                }
                if video.is_none() {
                    // 视频管线失效：降级纯色（P3/P6，不黑屏）
                    video = None;
                    if let Err(err) =
                        draw_solid_frame(&device, &context, &swapchain, fallback_color)
                    {
                        log::error!("wallpaper: solid fallback failed: {err}");
                    }
                }
            } else {
                // 纯色 / 图片模式无脏数据：阻塞等消息（stop / pause 经
                // PostThreadMessage 即时唤醒），空闲时线程纯睡眠、零轮询（P4）
                wait_for_message(Duration::from_millis(250));
            }
        }

        // 7. 清理：断开 visual 树并提交（仅 composition 路径）；关闭视频管线（MF 配对关闭）
        drop(video);
        if mf_inited {
            crate::video::VideoPipeline::shutdown_mf();
        }
        if let Some(tree) = &dcomp {
            let _ = tree.target.SetRoot(None);
            let _ = tree.device.Commit();
        }
        log::info!("wallpaper: render thread exited cleanly");
        Ok(())
    }
}

/// 宿主窗口客户区尺寸
unsafe fn host_client_size(hwnd: windows::Win32::Foundation::HWND) -> (f32, f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() && rect.right > 0 {
            (
                (rect.right - rect.left) as f32,
                (rect.bottom - rect.top) as f32,
            )
        } else {
            (1920.0, 1080.0)
        }
    }
}

/// 纯色帧：Clear 后缓冲 + VSync 提交
unsafe fn draw_solid_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    swapchain: &windows::Win32::Graphics::Dxgi::IDXGISwapChain1,
    color: [f32; 4],
) -> Result<(), String> {
    use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;

    unsafe {
        let back_buffer: ID3D11Texture2D = swapchain
            .GetBuffer(0)
            .map_err(|err| format!("GetBuffer failed: {err}"))?;
        // CreateRenderTargetView 为出参形式
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device
            .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))
            .map_err(|err| format!("CreateRenderTargetView failed: {err}"))?;
        let rtv = rtv.ok_or("CreateRenderTargetView returned no view")?;
        context.ClearRenderTargetView(&rtv, &color);
        // Present 返回裸 HRESULT：DXGI_ERROR 设备移除等场景需记录
        let hr = swapchain.Present(1, DXGI_PRESENT(0));
        if hr.is_err() {
            return Err(format!("Present failed: {hr:?}"));
        }
        Ok(())
    }
}

/// 图片帧：WIC 解码 → D2D bitmap → `DrawBitmap` 到交换链后缓冲（cover 缩放）。
///
/// 返回 `Ok(false)` 表示图片不可用（解码失败 / 尺寸非法），调用方降级纯色。
unsafe fn draw_image_frame(
    dxgi_device: &windows::Win32::Graphics::Dxgi::IDXGIDevice3,
    swapchain: &windows::Win32::Graphics::Dxgi::IDXGISwapChain1,
    path: &str,
    screen_w: f32,
    screen_h: f32,
) -> Result<bool, String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Bitmap1, ID2D1Factory1, D2D1_BITMAP_OPTIONS,
        D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
        D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        D2D1_INTERPOLATION_MODE_LINEAR,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::IDXGISurface;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    unsafe {
        // 1. D2D 工厂 / 设备 / 上下文（挂在壁纸的 DXGI 设备上，与 DComp 同源）
        let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|err| format!("D2D1CreateFactory failed: {err}"))?;
        let d2d_device = d2d_factory
            .CreateDevice(dxgi_device)
            .map_err(|err| format!("D2D CreateDevice failed: {err}"))?;
        let ctx = d2d_device
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
            .map_err(|err| format!("D2D CreateDeviceContext failed: {err}"))?;

        // 2. 交换链后缓冲包装为 D2D target bitmap
        let surface: IDXGISurface = swapchain
            .GetBuffer(0)
            .map_err(|err| format!("GetBuffer(IDXGISurface) failed: {err}"))?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS(
                D2D1_BITMAP_OPTIONS_TARGET.0 | D2D1_BITMAP_OPTIONS_CANNOT_DRAW.0,
            ),
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let target: ID2D1Bitmap1 = ctx
            .CreateBitmapFromDxgiSurface(&surface, Some(&props))
            .map_err(|err| format!("CreateBitmapFromDxgiSurface failed: {err}"))?;
        // SetTarget 收 ID2D1Image（COM 层级），显式上转
        let target_image = target
            .cast::<windows::Win32::Graphics::Direct2D::ID2D1Image>()
            .map_err(|err| format!("bitmap cast ID2D1Image failed: {err}"))?;
        ctx.SetTarget(Some(&target_image));

        // 3. WIC 解码（内置 JPG/PNG/BMP/TIFF/GIF；WebP/AVIF 依赖系统扩展，
        //    缺扩展时在下方统一降级纯色并说明原因）
        let wide: Vec<u16> = format!("{path}\0").encode_utf16().collect();
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| format!("CoCreateInstance(WICImagingFactory) failed: {err}"))?;
        let decode_result = wic.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        );
        let decoder = match decode_result {
            Ok(decoder) => decoder,
            Err(err) => {
                log::warn!("wallpaper: cannot decode '{path}' (missing codec or file?): {err}");
                return Ok(false);
            }
        };
        let frame = match decoder.GetFrame(0) {
            Ok(frame) => frame,
            Err(err) => {
                log::warn!("wallpaper: GetFrame(0) failed for '{path}': {err}");
                return Ok(false);
            }
        };
        let (mut img_w, mut img_h) = (0u32, 0u32);
        if frame.GetSize(&mut img_w, &mut img_h).is_err() {
            return Ok(false);
        }

        // 4. 像素格式统一到 32bppPBGRA（D2D 位图接受的格式）
        let converter = wic
            .CreateFormatConverter()
            .map_err(|err| format!("CreateFormatConverter failed: {err}"))?;
        if converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .is_err()
        {
            log::warn!("wallpaper: WIC format conversion unsupported for '{path}'");
            return Ok(false);
        }
        let bitmap = match ctx.CreateBitmapFromWicBitmap(&converter, None) {
            Ok(bitmap) => bitmap,
            Err(err) => {
                log::warn!("wallpaper: CreateBitmapFromWicBitmap failed: {err}");
                return Ok(false);
            }
        };

        // 5. cover 等比缩放（纯逻辑，见 geometry::tests）
        let Some((x, y, w, h)) = cover_rect(
            Size::new(screen_w, screen_h),
            Size::new(img_w as f32, img_h as f32),
        ) else {
            log::warn!("wallpaper: image '{path}' has degenerate size {img_w}x{img_h}");
            return Ok(false);
        };

        // 6. 绘制：黑底 + cover 绘制（黑底仅在浮点误差缝隙可见）。
        //    BeginDraw / DrawBitmap（DeviceContext 版）无返回值；EndDraw 汇总错误码。
        ctx.BeginDraw();
        ctx.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }));
        let dest = D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        ctx.DrawBitmap(
            &bitmap,
            Some(&dest),
            1.0,
            D2D1_INTERPOLATION_MODE_LINEAR,
            None,
            None,
        );
        ctx.EndDraw(None, None)
            .map_err(|err| format!("EndDraw failed: {err}"))?;

        // 7. VSync 提交
        let hr = swapchain.Present(1, windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0));
        if hr.is_err() {
            return Err(format!("Present failed: {hr:?}"));
        }
        Ok(true)
    }
}

/// 阻塞等待窗口 / 线程消息或超时（空闲时线程纯睡眠、零轮询唤醒，P4）。
/// 控制方法用 PostThreadMessage 提前唤醒；超时兜底保证停机标志的检查延迟。
fn wait_for_message(timeout: Duration) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MsgWaitForMultipleObjectsEx, MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS, QS_ALLINPUT,
    };
    let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    unsafe {
        let _ = MsgWaitForMultipleObjectsEx(
            None,
            ms,
            QS_ALLINPUT,
            MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
        );
    }
}
