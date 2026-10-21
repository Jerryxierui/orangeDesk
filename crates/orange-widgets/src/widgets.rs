//! 第一方组件（§3 B-3）：时钟、系统监控。
//! 天气 / 媒体控制 / 待办在后续批次接入（渲染层上线后）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::contract::{IWidget, OcclusionPolicy, WidgetError, WidgetFrame, WidgetMetadata};

/// 秒级时刻 → `HH:MM:SS`（UTC；本地时区由渲染层 / 真机阶段处理）
pub fn format_hms(secs_since_midnight: u64) -> String {
    let h = secs_since_midnight / 3600;
    let m = (secs_since_midnight % 3600) / 60;
    let s = secs_since_midnight % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 当前 UTC 秒级时刻（自午夜）
pub fn utc_secs_of_day() -> u64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs % 86_400
}

/// 极简时钟（1s 更新，遮挡暂停）
pub struct ClockWidget {
    frame: WidgetFrame,
}

impl ClockWidget {
    pub fn new() -> Self {
        Self {
            frame: WidgetFrame {
                lines: vec![format_hms(utc_secs_of_day())],
            },
        }
    }
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl IWidget for ClockWidget {
    fn metadata(&self) -> WidgetMetadata {
        WidgetMetadata {
            id: "clock",
            display_name: "Clock",
            update_interval: std::time::Duration::from_secs(1),
            occlusion_policy: OcclusionPolicy::Pause,
        }
    }

    fn update(&mut self, _now_ms: u64) -> Result<(), WidgetError> {
        self.frame = WidgetFrame {
            lines: vec![format_hms(utc_secs_of_day())],
        };
        Ok(())
    }

    fn suspend(&mut self) {}
    fn resume(&mut self) {}
    fn finalize(&mut self) {}
    fn frame(&self) -> WidgetFrame {
        self.frame.clone()
    }
}

/// 系统监控（2s 更新）：系统 CPU%（linux /proc/stat 差分）与内存（/proc/meminfo）。
/// 非 linux 平台读数标记 n/a（Windows 实现在真机阶段接入 PDH / 注册表）。
pub struct SysMonWidget {
    frame: WidgetFrame,
    #[cfg(target_os = "linux")]
    last_stat: Option<(u64, u64)>, // (busy, total) jiffies
}

impl SysMonWidget {
    pub fn new() -> Self {
        Self {
            frame: WidgetFrame::default(),
            #[cfg(target_os = "linux")]
            last_stat: None,
        }
    }
}

impl Default for SysMonWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl IWidget for SysMonWidget {
    fn metadata(&self) -> WidgetMetadata {
        WidgetMetadata {
            id: "sysmon",
            display_name: "System Monitor",
            update_interval: std::time::Duration::from_secs(2),
            occlusion_policy: OcclusionPolicy::Pause,
        }
    }

    fn update(&mut self, _now_ms: u64) -> Result<(), WidgetError> {
        let (cpu, mem_used_mb, mem_total_mb) = read_system_metrics(self);
        self.frame = WidgetFrame {
            lines: vec![
                format!("CPU  {cpu}"),
                format!("MEM  {mem_used_mb} / {mem_total_mb} MB"),
            ],
        };
        Ok(())
    }

    fn suspend(&mut self) {}
    fn resume(&mut self) {}
    fn finalize(&mut self) {}
    fn frame(&self) -> WidgetFrame {
        self.frame.clone()
    }
}

#[cfg(target_os = "linux")]
fn read_system_metrics(widget: &mut SysMonWidget) -> (String, u64, u64) {
    let cpu = (|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().next()?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse().ok())
            .collect();
        if fields.len() < 4 {
            return None;
        }
        // user nice system idle iowait irq softirq …（前 4 项足够估算）
        let busy = fields[0] + fields[1] + fields[2] + fields[4.min(fields.len() - 1)];
        let total: u64 = fields.iter().sum();
        let current = (busy, total);
        let delta = match widget.last_stat.replace(current) {
            Some((prev_busy, prev_total)) if total > prev_total => {
                let d_busy = busy.saturating_sub(prev_busy) as f64;
                let d_total = total.saturating_sub(prev_total) as f64;
                (d_busy / d_total * 100.0).round()
            }
            _ => 0.0,
        };
        Some(format!("{delta:.0}%"))
    })()
    .unwrap_or_else(|| "n/a".to_string());

    let mem = (|| {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb = |key: &str| -> Option<u64> {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        };
        let total = kb("MemTotal:")?;
        let available = kb("MemAvailable:")?;
        Some(((total - available) / 1024, total / 1024))
    })();

    match mem {
        Some((used, total)) => (cpu, used, total),
        None => (cpu, 0, 0),
    }
}

#[cfg(not(target_os = "linux"))]
fn read_system_metrics(_widget: &mut SysMonWidget) -> (String, u64, u64) {
    // Windows：真机阶段接入（GetSystemTimes / GlobalMemoryStatusEx）；
    // macOS 开发机：占位读数，链路验证用
    ("n/a".to_string(), 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_hms() {
        assert_eq!(format_hms(0), "00:00:00");
        assert_eq!(format_hms(3661), "01:01:01");
        assert_eq!(format_hms(86399), "23:59:59");
    }

    #[test]
    fn clock_widget_updates_frame() {
        let mut clock = ClockWidget::new();
        let before = clock.frame().lines[0].clone();
        clock.update(0).unwrap();
        let after = clock.frame().lines[0].clone();
        assert_eq!(after.len(), 8, "HH:MM:SS");
        assert_eq!(after.split(':').count(), 3);
        // 极小概率相等（跨秒边界），通常前值 ≠ 后值仅当跨秒——不强断言不等
        let _ = before;
    }

    #[test]
    fn clock_metadata_declares_1s_and_pause_policy() {
        let clock = ClockWidget::new();
        let meta = clock.metadata();
        assert_eq!(meta.id, "clock");
        assert_eq!(meta.update_interval, std::time::Duration::from_secs(1));
        assert_eq!(meta.occlusion_policy, OcclusionPolicy::Pause);
    }

    #[test]
    fn sysmon_metadata_declares_2s() {
        let sysmon = SysMonWidget::new();
        assert_eq!(
            sysmon.metadata().update_interval,
            std::time::Duration::from_secs(2)
        );
    }

    #[test]
    fn sysmon_update_produces_two_lines() {
        let mut sysmon = SysMonWidget::new();
        sysmon.update(0).unwrap();
        let frame = sysmon.frame();
        assert_eq!(frame.lines.len(), 2);
        assert!(frame.lines[0].starts_with("CPU"));
        assert!(frame.lines[1].starts_with("MEM"));
    }
}
