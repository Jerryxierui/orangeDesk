//! 多显示器 / DPI 摸底探测器（v0.1 §6「多显示器 / 混合 DPI 专项摸底」）。
//!
//! 数据类型与混合 DPI 判定是纯逻辑（跨平台可测）；
//! Windows 侧枚举（EnumDisplayMonitors + GetDpiForMonitor）cfg 隔离。
//! v0.4 才实现 per-monitor 独立壁纸，当前先收集拓扑并暴露风险信号。

/// 单台显示器条目（去掉 HWND 依赖的纯数据，便于日志与测试）
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorEntry {
    /// 设备名（\\.\DISPLAY1 等；摸底日志用）
    pub device: String,
    /// 桌面坐标矩形（虚拟屏幕坐标系）
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
    /// 有效 DPI（MDT_EFFECTIVE_DPI）
    pub dpi_x: u32,
    pub dpi_y: u32,
}

impl MonitorEntry {
    /// 缩放百分比（96 DPI 基准，100% / 125% / 150% ...）
    pub fn scale_percent(&self) -> u32 {
        if self.dpi_x == 0 {
            return 100;
        }
        self.dpi_x * 100 / 96
    }
}

/// 一次摸底快照
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorReport {
    pub monitors: Vec<MonitorEntry>,
}

impl MonitorReport {
    /// 混合 DPI：存在任意两台显示器缩放不同（DWM 渲染错位风险区，§9 风险表）
    pub fn mixed_dpi(&self) -> bool {
        let mut scales: Vec<u32> = self.monitors.iter().map(|m| m.scale_percent()).collect();
        scales.sort_unstable();
        scales.dedup();
        scales.len() > 1
    }

    /// 摸底报告（人类可读，直接打日志 / CI 归档）
    pub fn report_text(&self) -> String {
        let mut text = String::new();
        text.push_str(&format!(
            "monitor topology: {} display(s)\n",
            self.monitors.len()
        ));
        for m in &self.monitors {
            text.push_str(&format!(
                "  {} rect=({},{}) {}x{} primary={} scale={}%",
                m.device,
                m.x,
                m.y,
                m.w,
                m.h,
                m.primary,
                m.scale_percent()
            ));
        }
        if self.mixed_dpi() {
            text.push_str(
                "\n  [!] mixed-DPI setup detected (per-monitor DPI v2 handling required)",
            );
        }
        text
    }
}

#[cfg(windows)]
pub fn probe() -> MonitorReport {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MONITORINFOEXW};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    thread_local! {
        static FOUND: RefCell<Vec<windows::Win32::Graphics::Gdi::HMONITOR>> =
            const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn collect(
        hmonitor: windows::Win32::Graphics::Gdi::HMONITOR,
        _hdc: windows::Win32::Graphics::Gdi::HDC,
        _rect: *mut RECT,
        _data: LPARAM,
    ) -> windows::core::BOOL {
        FOUND.with(|f| f.borrow_mut().push(hmonitor));
        windows::core::BOOL(1)
    }

    let mut report = MonitorReport::default();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(0));
        let handles = FOUND.with(|f| f.borrow().clone());
        for hmon in handles {
            let mut info = MONITORINFOEXW {
                monitorInfo: windows::Win32::Graphics::Gdi::MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                    ..Default::default()
                },
                ..Default::default()
            };
            // cbSize 必须是 sizeof(MONITORINFOEXW)（szDevice 跟随其后），
            // GetMonitorInfoW 收 *mut MONITORINFO，用 embed 字段地址传入
            if !GetMonitorInfoW(hmon, &mut info.monitorInfo as *mut _).as_bool() {
                log::warn!("monitor probe: GetMonitorInfoW failed, skipping one display");
                continue;
            }
            // 摸底用每显示器有效 DPI；失败回退 96（探测不因 API 缺失而中断）
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            if GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
                log::warn!("monitor probe: GetDpiForMonitor failed, assuming 96 DPI");
            }
            let rc = info.monitorInfo.rcMonitor;
            let device_len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            report.monitors.push(MonitorEntry {
                device: String::from_utf16_lossy(&info.szDevice[..device_len]),
                x: rc.left,
                y: rc.top,
                w: rc.right - rc.left,
                h: rc.bottom - rc.top,
                primary: (info.monitorInfo.dwFlags & 1/* MONITORINFOF_PRIMARY */) != 0,
                dpi_x,
                dpi_y,
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(device: &str, x: i32, w: i32, dpi: u32, primary: bool) -> MonitorEntry {
        MonitorEntry {
            device: device.to_string(),
            x,
            y: 0,
            w,
            h: 1080,
            primary,
            dpi_x: dpi,
            dpi_y: dpi,
        }
    }

    #[test]
    fn scale_percent_maps_dpi() {
        assert_eq!(entry("", 0, 1920, 96, true).scale_percent(), 100);
        assert_eq!(entry("", 0, 1920, 120, true).scale_percent(), 125);
        assert_eq!(entry("", 0, 1920, 144, true).scale_percent(), 150);
        // DPI 为 0（探测失败占位）不应 panic
        assert_eq!(entry("", 0, 1920, 0, true).scale_percent(), 100);
    }

    #[test]
    fn mixed_dpi_detected_across_monitors() {
        let uniform = MonitorReport {
            monitors: vec![
                entry("d1", 0, 1920, 144, true),
                entry("d2", 1920, 1920, 144, false),
            ],
        };
        assert!(!uniform.mixed_dpi());

        let mixed = MonitorReport {
            monitors: vec![
                entry("d1", 0, 1920, 96, true),
                entry("d2", 1920, 1920, 144, false),
            ],
        };
        assert!(mixed.mixed_dpi());
        assert!(mixed.report_text().contains("mixed-DPI"));
    }

    #[test]
    fn report_text_lists_every_display() {
        let report = MonitorReport {
            monitors: vec![entry("\\\\.\\DISPLAY1", 0, 1920, 96, true)],
        };
        let text = report.report_text();
        assert!(text.contains("1 display(s)"));
        assert!(text.contains("\\\\.\\DISPLAY1"));
        assert!(text.contains("primary=true"));
        assert!(text.contains("scale=100%"));
    }
}
