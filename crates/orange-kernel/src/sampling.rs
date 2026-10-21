//! 进程级资源采样（§4 全产品预算的观察源；性能仪表盘数据）。
//!
//! CPU 为两次调用的差分（全机平均百分比，除以逻辑核数），
//! 内存为工作集 RSS。首次调用建立基线，返回 cpu=0 的读数。
//! 采样本身只在 supervisor 低频（秒级）触发，不构成轮询（P4）。

/// 一次进程级采样
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessSample {
    /// 全机平均 CPU 百分比（§4 的 <0.8% 即此口径）
    pub cpu_percent: f32,
    /// 工作集（RSS），MB
    pub working_set_mb: f32,
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn sample() -> Option<ProcessSample> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::SystemInformation::GetSystemInfo;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    // 差分基线（u64::MAX = 尚无基线）
    static LAST_CPU_100NS: AtomicU64 = AtomicU64::new(u64::MAX);
    static LAST_WALL_MS: AtomicU64 = AtomicU64::new(0);

    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).ok()?;
        let working_set_mb = counters.WorkingSetSize as f32 / (1024.0 * 1024.0);

        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
        let ft_100ns = |ft: &FILETIME| ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        let cpu_100ns = ft_100ns(&kernel).wrapping_add(ft_100ns(&user));
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;

        let prev_cpu = LAST_CPU_100NS.swap(cpu_100ns, Ordering::Relaxed);
        let prev_wall = LAST_WALL_MS.swap(wall_ms, Ordering::Relaxed);
        if prev_cpu == u64::MAX || wall_ms <= prev_wall {
            return Some(ProcessSample {
                cpu_percent: 0.0,
                working_set_mb,
            });
        }
        let mut info = Default::default();
        GetSystemInfo(&mut info);
        let cores = info.dwNumberOfProcessors.max(1) as f32;
        let delta_cpu_ms = (cpu_100ns - prev_cpu) as f32 / 10_000.0;
        let delta_wall_ms = (wall_ms - prev_wall) as f32;
        let cpu_percent = (delta_cpu_ms / delta_wall_ms / cores * 100.0).max(0.0);
        Some(ProcessSample {
            cpu_percent,
            working_set_mb,
        })
    }
}

// ---------------------------------------------------------------------------
// Linux（/proc；CI ubuntu 上仪表盘数据可用）
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub fn sample() -> Option<ProcessSample> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static LAST_CPU_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static LAST_WALL_MS: AtomicU64 = AtomicU64::new(0);

    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // comm 字段可含空格：从最后一个 ')' 之后解析字段（utime=14, stime=15，1-based）
    let after_comm = stat.rsplit(')').next()?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // rsplit 后 after_comm 以 " state ppid ..." 开头，utime 是原第 14 字段 = 索引 11
    let utime_ticks: u64 = fields.get(11)?.parse().ok()?;
    let stime_ticks: u64 = fields.get(12)?.parse().ok()?;
    let tick_hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let cpu_ms = ((utime_ticks + stime_ticks) as f64 * 1000.0 / tick_hz) as u64;

    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let working_set_mb = (resident_pages * page_size) as f32 / (1024.0 * 1024.0);

    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;

    let prev_cpu = LAST_CPU_MS.swap(cpu_ms, Ordering::Relaxed);
    let prev_wall = LAST_WALL_MS.swap(wall_ms, Ordering::Relaxed);
    if prev_cpu == u64::MAX || wall_ms <= prev_wall {
        return Some(ProcessSample {
            cpu_percent: 0.0,
            working_set_mb,
        });
    }
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f32;
    let delta_cpu_ms = (cpu_ms - prev_cpu) as f32;
    let delta_wall_ms = (wall_ms - prev_wall) as f32;
    let cpu_percent = (delta_cpu_ms / delta_wall_ms / cores * 100.0).max(0.0);
    Some(ProcessSample {
        cpu_percent,
        working_set_mb,
    })
}

// ---------------------------------------------------------------------------
// 其他平台（macOS 开发机）：无 /proc；仪表盘读数缺失但链路可用
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "linux")))]
pub fn sample() -> Option<ProcessSample> {
    None
}

#[cfg(any(windows, target_os = "linux"))]
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn sample_returns_memory_and_cpu_baseline_then_delta() {
        // 首次：建立基线（cpu 0.0，内存 > 0）
        let first = sample().expect("sampling works on this platform");
        assert!(first.working_set_mb > 0.0);
        assert_eq!(first.cpu_percent, 0.0);
        // 做点工作让 CPU 时间前进
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        assert!(acc < u64::MAX);
        thread::sleep(Duration::from_millis(50));
        let second = sample().expect("second sample");
        assert!(second.working_set_mb > 0.0);
        // CPU 百分比合法范围（全机平均）
        assert!(second.cpu_percent >= 0.0 && second.cpu_percent <= 100.0);
    }
}
