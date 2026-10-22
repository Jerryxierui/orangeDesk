//! `SetWindowCompositionAttribute` 封装（§3 模块 C 主路径，TranslucentTB 同款）。
//!
//! 这是 undocumented API：无导入库符号，必须运行时 `GetProcAddress` 动态解析。
//! 能力探测纪律（P3/P6）：
//! - 函数解析失败（未来 Windows 版本移除）→ 一切效果不可用 → 仅 Native
//! - 应用失败时按 Acrylic → Blur → Clear → Native 逐级回落，
//!   原因记录在 `last_note` 供 status / 仪表盘展示，绝不静默无效

use windows::core::PCSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, FindWindowW, GetClassNameW};

use crate::rules::Material;

/// WINDOWCOMPOSITIONATTRIB::WCA_ACCENT_POLICY
const WCA_ACCENT_POLICY: u32 = 3;
/// ACCENT_STATE 值（undocumented，Win10 1803+ 语义稳定）
const ACCENT_DISABLE: i32 = 0;
const ACCENT_ENABLE_TRANSPARENTGRADIENT: i32 = 2; // Clear
const ACCENT_ENABLE_BLURBEHIND: i32 = 3; // Blur
const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4; // Acrylic

#[repr(C)]
struct WindowCompositionAttribData {
    attribute: u32,
    data: *mut core::ffi::c_void,
    size_of_data: usize,
}

#[repr(C)]
struct AccentPolicy {
    accent_state: i32,
    accent_flags: u32,
    /// 0xAABBGGRR 布局的着色
    gradient_color: u32,
    animation_id: u32,
}

type ApplyFn = unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

/// composition 能力封装（一次解析，重复使用；线程亲和：user32 调用同线程即可）
pub struct Composition {
    apply_fn: Option<ApplyFn>,
    /// 最近一次降级说明（status 展示；空 = 一切正常）
    pub last_note: String,
}

impl Composition {
    pub fn new() -> Self {
        let apply_fn = unsafe { resolve_apply_fn() };
        let mut composition = Self {
            apply_fn,
            last_note: String::new(),
        };
        if composition.apply_fn.is_none() {
            composition.last_note =
                "SetWindowCompositionAttribute unavailable on this system; native only".to_string();
        }
        composition
    }

    /// 应用材质到单个窗口；能力探测降级链在此发生，
    /// 返回最终成功应用的材质（全部失败 → Native 并记录原因）。
    pub fn apply(&mut self, hwnd: HWND, material: Material, tint_abgr: u32) -> Material {
        let Some(apply_fn) = self.apply_fn else {
            return Material::Native;
        };
        // 降级链：目标材质起逐级回落（Acrylic → Blur → Clear）
        let chain: &[Material] = match material {
            Material::Acrylic => &[Material::Acrylic, Material::Blur, Material::Clear],
            Material::Blur => &[Material::Blur, Material::Clear],
            Material::Clear => &[Material::Clear],
            // Native = 主动恢复系统原生（关闭 accent）
            Material::Native => &[Material::Native],
        };
        for candidate in chain {
            if unsafe { apply_material(apply_fn, hwnd, *candidate, tint_abgr) } {
                if *candidate != material {
                    self.last_note = format!(
                        "{material:?} unavailable on this system, fell back to {candidate:?}"
                    );
                } else {
                    self.last_note.clear();
                }
                return *candidate;
            }
        }
        self.last_note = format!("composition apply failed for all candidates of {material:?}");
        Material::Native
    }
}

impl Default for Composition {
    fn default() -> Self {
        Self::new()
    }
}

unsafe fn resolve_apply_fn() -> Option<ApplyFn> {
    unsafe {
        let module: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        let module = GetModuleHandleW(windows::core::PCWSTR(module.as_ptr())).ok()?;
        let symbol = PCSTR(c"SetWindowCompositionAttribute".as_ptr().cast::<u8>());
        // FARPROC = Option<unsafe extern "system" fn() -> isize>
        let addr = GetProcAddress(module, symbol)?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            ApplyFn,
        >(addr))
    }
}

unsafe fn apply_material(
    apply_fn: ApplyFn,
    hwnd: HWND,
    material: Material,
    tint_abgr: u32,
) -> bool {
    let accent_state = match material {
        Material::Native => ACCENT_DISABLE,
        Material::Clear => ACCENT_ENABLE_TRANSPARENTGRADIENT,
        Material::Blur => ACCENT_ENABLE_BLURBEHIND,
        Material::Acrylic => ACCENT_ENABLE_ACRYLICBLURBEHIND,
    };
    // Acrylic / Blur / Clear 都带统一着色；Native 无视 tint
    let mut policy = AccentPolicy {
        accent_state,
        accent_flags: 0,
        gradient_color: if material == Material::Native {
            0
        } else {
            tint_abgr
        },
        animation_id: 0,
    };
    let mut data = WindowCompositionAttribData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut core::ffi::c_void,
        size_of_data: std::mem::size_of::<AccentPolicy>(),
    };
    // API 返回 BOOL：非 0 为成功
    unsafe { apply_fn(hwnd, &mut data) != 0 }
}

/// 定位任务栏窗口：主任务栏 Shell_TrayWnd + 多显示器次要任务栏 Shell_SecondaryTrayWnd。
/// explorer 重启后必须重新调用（旧句柄失效，模块 C 纪律）。
pub fn locate_taskbar_windows() -> Vec<HWND> {
    use std::cell::RefCell;
    thread_local! {
        static FOUND: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn collect(
        hwnd: HWND,
        _lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::core::BOOL {
        if class_name_is(hwnd, "Shell_SecondaryTrayWnd") {
            FOUND.with(|f| f.borrow_mut().push(hwnd));
        }
        windows::core::BOOL(1)
    }

    unsafe fn class_name_is(hwnd: HWND, expected: &str) -> bool {
        let mut buf = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        if len <= 0 {
            return false;
        }
        let name = String::from_utf16_lossy(&buf[..len as usize]);
        name == expected
    }

    unsafe {
        let mut windows = Vec::new();
        // 清理上一轮收集：explorer 重启后 relocate 在同线程再次枚举，
        // 不清会累积失效句柄导致重复应用
        FOUND.with(|f| f.borrow_mut().clear());
        // FindWindowW(lpclassname, lpwindowname)：按类名找主任务栏
        let class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        if let Ok(hwnd) = FindWindowW(windows::core::PCWSTR(class.as_ptr()), None) {
            if !hwnd.is_invalid() {
                windows.push(hwnd);
            }
        }
        // 多显示器次要任务栏：枚举顶层窗口按类名收集
        let _ = EnumWindows(Some(collect), windows::Win32::Foundation::LPARAM(0));
        windows
    }
}
