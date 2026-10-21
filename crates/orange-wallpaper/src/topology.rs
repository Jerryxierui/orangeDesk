//! 桌面拓扑探测器（§3 A-0：一等公民，不做则在 Win11 24H2+ 上黑屏）。
//!
//! 运行时判定三类路径：
//! - `Classic`：Win10 / Win11 ≤23H2。向 Progman 发送 `0x052C`（ SpawnWorkerW）
//!   后出现 WorkerW，壁纸窗口 SetParent 到 WorkerW
//! - `Raised`：Win11 24H2+（build ≥ 26100）。Progman 变为
//!   `WS_EX_NOREDIRECTIONBITMAP`，WorkerW 路径失效，壁纸作为 Progman
//!   子窗口直挂 + DirectComposition 提交（FeatherWall 实测结论）
//! - `Fallback`：以上皆失败 → 普通置底窗口（功能受限，明确提示，不静默）

use std::cell::RefCell;
use std::thread;
use std::time::Duration;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetClassNameW, GetWindow, GetWindowLongPtrW, SendMessageTimeoutW,
    GWL_EXSTYLE, GW_HWNDNEXT, SMTO_NORMAL, WS_EX_NOREDIRECTIONBITMAP,
};

/// 探测结果：桌面窗口层级的挂载路径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopTopology {
    /// WorkerW 存在（Win10 / Win11 ≤23H2 主路径）
    Classic { worker_w: HWND },
    /// 24H2+：Progman NOREDIRECTIONBITMAP，子窗口直挂
    Raised { host: HWND },
    /// 回退路径：普通置底窗口（v0.1 骨架未实现，调用方给出明确提示）
    Fallback,
}

// SAFETY: HWND 是系统全局句柄（Win32 句柄表语义），跨线程传递安全；
// 探测在主线程、挂载与渲染在渲染线程，必须跨线程传递。
unsafe impl Send for DesktopTopology {}

/// 通知 shell 创建 / 刷新 WorkerW 的未公开消息（业界通用做法）
const WM_SPAWN_WORKER_W: u32 = 0x052C;
/// 0x052C 消息参数：0x1D = 创建 WorkerW（其他值用于其它 shell 行为）
const SPAWN_FLAG: isize = 0x1D;
/// 消息重试次数与间隔（文档 A-0：发送 0x052C 并重试）
const SPAWN_RETRIES: u32 = 3;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(120);
/// Win11 24H2 起始 build（26100）
const BUILD_24H2: u32 = 26100;

/// 运行时探测桌面拓扑。
pub fn probe() -> DesktopTopology {
    unsafe {
        let progman = find_progman();
        let Some(progman) = progman else {
            log::error!("topology: Progman not found; no shell desktop");
            return DesktopTopology::Fallback;
        };

        // Classic 路径：0x052C + 重试，然后找 WorkerW
        for _ in 0..SPAWN_RETRIES {
            let mut send_result = 0usize;
            let _ = SendMessageTimeoutW(
                progman,
                WM_SPAWN_WORKER_W,
                WPARAM(0),
                LPARAM(SPAWN_FLAG),
                SMTO_NORMAL,
                200,
                Some(&mut send_result),
            );
            if let Some(worker_w) = find_worker_w(progman) {
                log::info!("topology: Classic (WorkerW found)");
                return DesktopTopology::Classic { worker_w };
            }
            thread::sleep(SPAWN_RETRY_DELAY);
        }

        // Raised 路径（24H2+）：WorkerW 不出现 → Progman 子窗口直挂。
        // 以 build 号 + NOREDIRECTIONBITMAP 扩展样式双重判定，避免单点误判。
        let build = os_build_number();
        let no_redirection = (GetWindowLongPtrW(progman, GWL_EXSTYLE) as usize
            & WS_EX_NOREDIRECTIONBITMAP.0 as usize)
            != 0;
        if build.map(|b| b >= BUILD_24H2).unwrap_or(false) || no_redirection {
            log::info!(
                "topology: Raised (24H2+, build={build:?}, no_redirection_bitmap={no_redirection})"
            );
            return DesktopTopology::Raised { host: progman };
        }

        log::warn!("topology: no mountable path found; fallback (limited functionality)");
        DesktopTopology::Fallback
    }
}

unsafe fn find_progman() -> Option<HWND> {
    let class: Vec<u16> = "Progman\0".encode_utf16().collect();
    let title: Vec<u16> = "Program Manager\0".encode_utf16().collect();
    unsafe {
        let hwnd = FindWindowW(
            windows::core::PCWSTR(class.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
        )
        .ok()?;
        if hwnd.is_invalid() {
            None
        } else {
            Some(hwnd)
        }
    }
}

/// 标准 WorkerW 定位法（Wallpaper Engine / Lively 同款）：
/// 枚举顶层窗口找到 `SHELLDLL_DefView`（桌面图标视图宿主，父为 Progman
/// 或另一个 WorkerW），其 `GW_HWNDNEXT` 兄弟窗口即壁纸 WorkerW。
unsafe fn find_worker_w(_progman: HWND) -> Option<HWND> {
    thread_local! {
        static CANDIDATES: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn collect_children(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        CANDIDATES.with(|c| c.borrow_mut().push(hwnd));
        BOOL(1)
    }

    CANDIDATES.with(|c| c.borrow_mut().clear());
    unsafe {
        let _ = EnumWindows(Some(collect_children), LPARAM(0));
        let candidates = CANDIDATES.with(|c| c.borrow().clone());
        for hwnd in candidates {
            if class_name(hwnd) != "SHELLDLL_DefView" {
                continue;
            }
            let sibling = GetWindow(hwnd, GW_HWNDNEXT).ok()?;
            if !sibling.is_invalid() && class_name(sibling) == "WorkerW" {
                return Some(sibling);
            }
        }
    }
    None
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 读注册表 CurrentBuildNumber（GetVersionEx 受 manifest 谎报限制，不用）
unsafe fn os_build_number() -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "CurrentBuildNumber\0".encode_utf16().collect();
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = (size as usize) / 2;
    let text = String::from_utf16_lossy(&buf[..len]);
    text.trim_end_matches('\0').trim().parse().ok()
}

/// build 号解析（独立纯函数便于测试）
#[cfg(test)]
mod tests {
    #[test]
    fn build_string_parses() {
        assert_eq!(
            Some(26100),
            "26100\0".trim_end_matches('\0').trim().parse().ok()
        );
        assert_eq!(Some(19045), " 19045 ".trim().parse().ok());
        assert_eq!(None, "abc".parse::<u32>().ok());
    }
}
