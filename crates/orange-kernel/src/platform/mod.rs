//! 平台事件源：把操作系统通知翻译成 [`KernelEvent`] 注入总线（P4）。
//!
//! Windows：message-only 窗口 + WinEventHook + WTS 会话通知 + 电源通知，
//! 全部事件驱动，无任何轮询（对齐 TranslucentTB <10ms 响应纪律）。
//! 非 Windows：占位事件源（本项目 Windows 单平台优先，见设计文档 §10），
//! 供内核链路在开发机 / CI 上验证。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::bus::EventPublisher;

/// 全局停机标志：Ctrl+C / 控制台事件 / supervisor 共同置位。
/// 信号处理器上下文只能做原子写，因此用进程级静态变量。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// 平台事件源契约：阻塞运行，直到 [`shutdown_requested`]。
pub trait EventSource: Send + 'static {
    fn run(&mut self, publisher: EventPublisher);
}

/// 按平台构造真实事件源。
pub fn platform_event_source() -> Box<dyn EventSource> {
    #[cfg(windows)]
    {
        Box::new(windows_impl::WindowsEventSource::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(unix::IdleEventSource)
    }
}

/// 安装 Ctrl+C / 控制台关闭处理器（跨平台，置位全局停机标志）。
pub fn install_ctrl_handler() -> Result<(), String> {
    #[cfg(windows)]
    {
        windows_impl::install_ctrl_handler_impl()
    }
    #[cfg(not(windows))]
    {
        unix::install_ctrl_handler_impl()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("unsupported platform".to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod windows_impl {
    use std::cell::{Cell, RefCell};
    use std::thread;
    use std::time::Duration;

    use crate::bus::EventPublisher;
    use orange_module_api::KernelEvent;

    // `::windows` 绝对路径：避免与模块名解析歧义
    use ::windows::core::{BOOL, PCWSTR, PWSTR};
    use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use ::windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use ::windows::Win32::System::Power::{
        RegisterPowerSettingNotification, POWERBROADCAST_SETTING,
    };
    use ::windows::Win32::System::RemoteDesktop::WTSRegisterSessionNotification;
    use ::windows::Win32::System::SystemServices::{
        GUID_ACDC_POWER_SOURCE, GUID_POWER_SAVING_STATUS,
    };
    use ::windows::Win32::System::Threading::GetCurrentThreadId;
    use ::windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongW,
        GetWindowRect, GetWindowThreadProcessId, PostMessageW, PostThreadMessageW, RegisterClassW,
        RegisterWindowMessageW, EVENT_SYSTEM_FOREGROUND, GWL_STYLE, HWND_MESSAGE,
        REGISTER_NOTIFICATION_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT,
        WINEVENT_SKIPOWNPROCESS, WM_APP, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WM_QUIT,
        WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
    };

    /// 自定义消息：WinEventHook 回调 → 消息窗口（避免在 hook 上下文做重活）
    const WM_APP_FOREGROUND: u32 = WM_APP + 1;
    /// 电源设置变更通知（windows-rs 未导出该常量，值为 WinUser.h 中的 0x8013）
    const PBT_POWERSETTING_CHANGE: u32 = 0x8013;

    /// Windows 事件源：在专用线程上运行 message-only 窗口的消息循环。
    pub struct WindowsEventSource {
        thread_id: std::sync::Arc<std::sync::Mutex<Option<u32>>>,
    }

    impl WindowsEventSource {
        pub fn new() -> Self {
            Self {
                thread_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            }
        }
    }

    impl Default for WindowsEventSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl super::EventSource for WindowsEventSource {
        fn run(&mut self, publisher: EventPublisher) {
            let thread_id = self.thread_id.clone();
            let loop_thread_id = thread_id.clone();
            let handle = thread::Builder::new()
                .name("od-platform-events".to_string())
                .spawn(move || run_message_loop(publisher, loop_thread_id))
                .expect("spawn platform event thread");

            while !super::shutdown_requested() {
                thread::sleep(Duration::from_millis(200));
            }
            // 通知消息循环退出（WM_QUIT 到指定线程）
            if let Some(tid) = *thread_id.lock().unwrap() {
                unsafe {
                    let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                }
            }
            let _ = handle.join();
        }
    }

    /// 消息线程的线程本地状态
    struct SourceState {
        publisher: EventPublisher,
        /// 本线程消息窗口（WinEventHook 回调转发的目标）
        msg_hwnd: Cell<Option<HWND>>,
        taskbar_created_msg: Cell<u32>,
        last_fullscreen: Cell<bool>,
        last_foreground_process: RefCell<String>,
    }

    thread_local! {
        static STATE: RefCell<Option<SourceState>> = const { RefCell::new(None) };
    }

    fn publish(event: KernelEvent) {
        STATE.with(|s| {
            if let Some(state) = s.borrow().as_ref() {
                state.publisher.publish(event);
            }
        });
    }

    fn run_message_loop(
        publisher: EventPublisher,
        thread_id_out: std::sync::Arc<std::sync::Mutex<Option<u32>>>,
    ) {
        unsafe {
            let class_name: Vec<u16> = "orangeDeskEventSource\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                log::error!(
                    "RegisterClassW failed: {:?}",
                    ::windows::Win32::Foundation::GetLastError()
                );
                return;
            }

            // message-only 窗口：不可见、不参与桌面合成、只收消息
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(class_name.as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(err) => {
                    log::error!("CreateWindowExW failed: {err}");
                    return;
                }
            };

            // 锁屏 / 解锁（WTS 会话通知）
            if WTSRegisterSessionNotification(hwnd, 0 /* NOTIFY_FOR_THIS_SESSION */).is_ok() {
                log::info!("WTS session notifications registered");
            } else {
                log::warn!("WTSRegisterSessionNotification failed (lock screen events disabled)");
            }

            // 电源通知（节能模式 / 电池 ↔ 外接）
            for guid in [&GUID_POWER_SAVING_STATUS, &GUID_ACDC_POWER_SOURCE] {
                if RegisterPowerSettingNotification(
                    hwnd.into(),
                    guid,
                    REGISTER_NOTIFICATION_FLAGS(0),
                )
                .is_err()
                {
                    log::warn!("RegisterPowerSettingNotification failed for {guid:?}");
                }
            }

            // 前台窗口变化（WinEventHook，事件驱动而非轮询）
            let hook: HWINEVENTHOOK = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.is_invalid() {
                log::warn!("SetWinEventHook failed (foreground events disabled)");
            }

            let taskbar_name: Vec<u16> = "TaskbarCreated\0".encode_utf16().collect();
            let taskbar_created = RegisterWindowMessageW(PCWSTR(taskbar_name.as_ptr()));

            STATE.with(|s| {
                *s.borrow_mut() = Some(SourceState {
                    publisher,
                    msg_hwnd: Cell::new(Some(hwnd)),
                    taskbar_created_msg: Cell::new(taskbar_created),
                    last_fullscreen: Cell::new(false),
                    last_foreground_process: RefCell::new(String::new()),
                })
            });

            *thread_id_out.lock().unwrap() = Some(GetCurrentThreadId());
            log::info!("platform event source ready (message loop running)");

            let mut msg = Default::default();
            loop {
                // BOOL: >0 有消息 / 0 = WM_QUIT / -1 = 错误
                let ret = GetMessageW(&mut msg, None, 0, 0);
                if ret.0 <= 0 {
                    break;
                }
                let _ = DispatchMessageW(&msg);
            }

            if !hook.is_invalid() {
                let _ = UnhookWinEvent(hook);
            }
            STATE.with(|s| *s.borrow_mut() = None);
            log::info!("platform event source stopped");
        }
    }

    /// WinEventHook 回调：只做 PostMessage 转发，处理在窗口过程完成。
    unsafe extern "system" fn win_event_proc(
        _hook: HWINEVENTHOOK,
        _event: u32,
        hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        STATE.with(|s| {
            if let Some(state) = s.borrow().as_ref() {
                if let Some(msg_hwnd) = state.msg_hwnd.get() {
                    unsafe {
                        // WPARAM 携带前台窗口句柄
                        let _ = PostMessageW(
                            Some(msg_hwnd),
                            WM_APP_FOREGROUND,
                            WPARAM(hwnd.0 as usize),
                            LPARAM(0),
                        );
                    }
                }
            }
        });
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let taskbar_created = STATE.with(|s| {
            s.borrow()
                .as_ref()
                .map(|state| state.taskbar_created_msg.get())
                .unwrap_or(0)
        });

        match msg {
            WM_APP_FOREGROUND => {
                let foreground = HWND(wparam.0 as *mut _);
                handle_foreground(foreground);
                LRESULT(0)
            }
            m if taskbar_created != 0 && m == taskbar_created => {
                // explorer.exe（重新）启动后的系统广播
                publish(KernelEvent::ShellRestarted);
                LRESULT(0)
            }
            WM_WTSSESSION_CHANGE => {
                let locked = wparam.0 == WTS_SESSION_LOCK as usize;
                let unlocked = wparam.0 == WTS_SESSION_UNLOCK as usize;
                if locked || unlocked {
                    publish(KernelEvent::LockScreenChanged { locked });
                }
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                if wparam.0 as u32 == PBT_POWERSETTING_CHANGE {
                    let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
                    let data = setting.Data[0];
                    if setting.PowerSetting == GUID_POWER_SAVING_STATUS {
                        publish(KernelEvent::PowerSaverChanged { enabled: data != 0 });
                    } else if setting.PowerSetting == GUID_ACDC_POWER_SOURCE {
                        // 0 = 外接电源, 1 = 电池
                        publish(KernelEvent::PowerSourceChanged {
                            on_battery: data != 0,
                        });
                    }
                }
                LRESULT(1)
            }
            WM_DISPLAYCHANGE => {
                publish(KernelEvent::DisplayChanged);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 前台变化处理：进程名（尽力解析）+ 全屏判定，均做去重后发布
    fn handle_foreground(hwnd: HWND) {
        let (process_name, fullscreen) = unsafe { inspect_foreground(hwnd) };
        let mut foreground_event = None;
        let mut fullscreen_event = None;
        STATE.with(|s| {
            if let Some(state) = s.borrow().as_ref() {
                let mut last_proc = state.last_foreground_process.borrow_mut();
                if *last_proc != process_name {
                    *last_proc = process_name.clone();
                    foreground_event = Some(KernelEvent::ForegroundChanged { process_name });
                }
                if state.last_fullscreen.get() != fullscreen {
                    state.last_fullscreen.set(fullscreen);
                    fullscreen_event = Some(KernelEvent::FullscreenChanged { fullscreen });
                }
            }
        });
        // 先前台变化再全屏变化：模块（如壁纸）按全屏事件决策暂停
        if let Some(event) = foreground_event {
            publish(event);
        }
        if let Some(event) = fullscreen_event {
            publish(event);
        }
    }

    /// 取前台窗口信息：进程名（尽力）+ 是否全屏独占
    /// （无边框 && 客户矩形覆盖所在显示器整块）
    unsafe fn inspect_foreground(hwnd: HWND) -> (String, bool) {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let mut process_name = String::new();
            let mut pid = 0u32;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    let mut buf = [0u16; 512];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(
                        handle,
                        PROCESS_NAME_FORMAT(0),
                        PWSTR(buf.as_mut_ptr()),
                        &mut len,
                    )
                    .is_ok()
                    {
                        if let Some(pos) = buf[..len as usize].iter().position(|&c| c == 0) {
                            process_name = String::from_utf16_lossy(&buf[..pos]);
                        }
                    }
                    let _ = CloseHandle(handle);
                }
            }

            let mut rect = RECT::default();
            let fullscreen = if GetWindowRect(hwnd, &mut rect).is_ok() {
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFO).as_bool() {
                    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
                    let covers = rect.left <= info.rcMonitor.left
                        && rect.top <= info.rcMonitor.top
                        && rect.right >= info.rcMonitor.right
                        && rect.bottom >= info.rcMonitor.bottom;
                    // 带系统边框的“最大化”不算全屏独占；无边框且覆盖整屏才算
                    covers && (style & WS_OVERLAPPEDWINDOW.0) != WS_OVERLAPPEDWINDOW.0
                } else {
                    false
                }
            } else {
                false
            };
            (process_name, fullscreen)
        }
    }

    pub fn install_ctrl_handler_impl() -> Result<(), String> {
        unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
            // CTRL_C / CTRL_CLOSE / CTRL_LOGOFF / CTRL_SHUTDOWN 统一处理
            super::request_shutdown();
            BOOL(1)
        }
        unsafe {
            use ::windows::Win32::System::Console::SetConsoleCtrlHandler;
            SetConsoleCtrlHandler(Some(handler), true)
                .map_err(|err| format!("SetConsoleCtrlHandler: {err}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Unix 占位实现（开发 / CI 链路验证）
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod unix {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// 占位事件源：orangeDesk 是 Windows 单平台产品（§10），
    /// 非 Windows 上仅维持链路（配置/注册/总线/降级路径）可运行、可测试。
    pub struct IdleEventSource;

    impl EventSource for IdleEventSource {
        fn run(&mut self, _publisher: EventPublisher) {
            log::info!("platform event source: idle (non-Windows placeholder)");
            while !shutdown_requested() {
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    pub fn install_ctrl_handler_impl() -> Result<(), String> {
        unsafe extern "C" fn on_signal(_sig: i32) {
            // 信号上下文只允许原子写
            request_shutdown();
        }
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as usize);
            libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        }
        Ok(())
    }
}
