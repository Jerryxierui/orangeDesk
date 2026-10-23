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
    CreateWindowExW, DefWindowProcW, GetWindowRect, RegisterClassW, SetWindowPos, HWND_BOTTOM,
    SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_SHOWWINDOW, WM_NCACTIVATE, WNDCLASSW, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_VISIBLE,
};

use crate::topology::DesktopTopology;

const CLASS_NAME: &str = "orangeDeskWallpaperHost\0";

/// 目标区域（虚拟屏幕坐标；F1b 多显示器：每实例定位到自己的显示器矩形）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 在调用线程上创建壁纸宿主子窗口并挂载到拓扑给出的父窗口，
/// 定位到 `rect`（虚拟屏幕坐标；坐标系换算见函数体注释）。
///
/// 必须在渲染线程调用（窗口线程相关性：DComp target 与消息泵同线程）。
pub fn create_wallpaper_child(
    topology: &DesktopTopology,
    rect: TargetRect,
) -> Result<HWND, String> {
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

        // 子窗口坐标相对父窗口客户区原点；WorkerW/Progman 覆盖虚拟屏幕且
        // 无边框，客户区原点 = 父窗口原点（虚拟屏幕左上角，可能为负）。
        // 用父窗口原点做偏移换算（真机校准点：若 24H2 Progman 原点语义
        // 不同，仅此一处需要调整）。
        let mut parent_rect = RECT::default();
        let (offset_x, offset_y) = if GetWindowRect(parent, &mut parent_rect).is_ok() {
            (parent_rect.left, parent_rect.top)
        } else {
            (0, 0)
        };
        let x = rect.x - offset_x;
        let y = rect.y - offset_y;
        if rect.width <= 0 || rect.height <= 0 {
            return Err(format!(
                "target rect for wallpaper host is empty ({}x{} at {},{})",
                rect.width, rect.height, rect.x, rect.y
            ));
        }

        let title: Vec<u16> = "orangeDeskWallpaper\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            x,
            y,
            rect.width,
            rect.height,
            Some(parent),
            None,
            None,
            None,
        )
        .map_err(|err| format!("CreateWindowExW failed for wallpaper host: {err}"))?;

        // 压到底部，避免遮挡桌面图标层（SHELLDLL_DefView）。
        // NOSIZE|NOMOVE 必须给：x/y/cx/cy 占位 0，无这两个 flag 会被解读为
        // “移动到 (0,0) 并缩为 0×0”（真机上壁纸不可见的隐性 bug）
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SET_WINDOW_POS_FLAGS(
                SWP_NOACTIVATE.0
                    | SWP_NOOWNERZORDER.0
                    | SWP_SHOWWINDOW.0
                    | SWP_NOSIZE.0
                    | SWP_NOMOVE.0,
            ),
        );
        Ok(hwnd)
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
