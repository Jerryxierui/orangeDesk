//! 壁纸窗口挂载（§3 A-0 双路径）。
//!
//! - Classic：child 窗口 SetParent 到 WorkerW（历史主路径）
//! - Raised：child 窗口 SetParent 到 Progman（24H2+，配合 DComp 提交）
//!
//! 窗口属性：不可激活、不抢焦点、无标题边框；真正的像素内容由
//! renderer 的 DirectComposition visual 提供（本窗口只是挂载锚点 + 消息泵宿主）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetSystemMetrics, RegisterClassW, SetWindowPos,
    HWND_BOTTOM, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW,
    SYSTEM_METRICS_INDEX, WM_NCACTIVATE, WNDCLASSW, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_VISIBLE,
};

use crate::topology::DesktopTopology;

const CLASS_NAME: &str = "orangeDeskWallpaperHost\0";

/// 在调用线程上创建壁纸宿主子窗口并挂载到拓扑给出的父窗口。
///
/// 必须在渲染线程调用（窗口线程相关性：DComp target 与消息泵同线程）。
pub fn create_wallpaper_child(topology: &DesktopTopology) -> Result<HWND, String> {
    unsafe {
        let parent = match topology {
            DesktopTopology::Classic { worker_w } => *worker_w,
            DesktopTopology::Raised { host } => *host,
            DesktopTopology::Fallback => {
                // 回退路径（普通置底顶层窗口）在 v0.1 骨架中明确未实现：
                // 宁可报“功能受限”也不静默出错（P3/P6）
                return Err(
                    "no classic/raised mount point on this system; wallpaper is limited (fallback path pending)"
                        .to_string(),
                );
            }
        };

        let class_name: Vec<u16> = CLASS_NAME.encode_utf16().collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_wnd_proc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // 重复注册（explorer 重启 / remount）返回 0，可安全忽略
        let _ = RegisterClassW(&wc);

        // 覆盖宿主（v0.1：主显示器；多显示器独立壁纸是 v0.4 范围）
        let (width, height) = parent_client_size(parent);
        if width == 0 || height == 0 {
            return Err(format!(
                "mount parent has empty client area ({width}x{height})"
            ));
        }

        let title: Vec<u16> = "orangeDeskWallpaper\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            width,
            height,
            Some(parent),
            None,
            None,
            None,
        )
        .map_err(|err| format!("CreateWindowExW failed for wallpaper host: {err}"))?;

        // 压到底部，避免遮挡桌面图标层（SHELLDLL_DefView）
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_NOOWNERZORDER.0 | SWP_SHOWWINDOW.0),
        );
        Ok(hwnd)
    }
}

/// 宿主父窗口客户区尺寸；取不到时回退主屏幕尺寸
unsafe fn parent_client_size(parent: HWND) -> (i32, i32) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(parent, &mut rect).is_ok() && rect.right > 0 {
            return (rect.right, rect.bottom);
        }
        // SM_CXSCREEN=0 / SM_CYSCREEN=1
        (
            GetSystemMetrics(SYSTEM_METRICS_INDEX(0)),
            GetSystemMetrics(SYSTEM_METRICS_INDEX(1)),
        )
    }
}

unsafe extern "system" fn host_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 不抢激活：壁纸窗口永远不应成为前台 / 抢焦点
    if msg == WM_NCACTIVATE {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
